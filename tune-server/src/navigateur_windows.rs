//! #9061 — ouvrir l'interface sous Windows quand aucun navigateur n'est
//! défini par défaut.
//!
//! Testeur Windows 10 (18/11) : au lancement, Windows affichait
//! « http://localhost:8888 — Application introuvable ». Le serveur tournait :
//! la même adresse tapée dans Firefox ouvrait Tune. L'ouverture passait par
//! `cmd /C start "" <url>`, c'est-à-dire par l'association du protocole
//! `http` : sur ce poste, elle ne menait à aucun programme. Avec le lancement
//! sans console (#7637), cette fenêtre d'erreur était le SEUL retour visible —
//! le testeur a conclu que le serveur ne tournait pas et il est revenu à la
//! rc10.
//!
//! Désormais :
//!
//! 1. l'association est lue par le shell lui-même (`AssocQueryStringW`,
//!    `ASSOCSTR_EXECUTABLE`) : pour le `ProgId` choisi par l'utilisateur
//!    (`UserChoice`) s'il y en a un — et SEULEMENT pour lui, jamais un repli
//!    sur `HKCR\http`, qui nomme souvent un iexplore encore présent alors que
//!    le shell, lui, suit le choix cassé —, sinon pour le protocole `http` ;
//!    un `ProgId` sans exécutable mais dont la clé `HKCR\<ProgId>` existe
//!    (application du Store, Edge ancien) compte comme valide ;
//! 2. l'ouverture par l'association passe par `ShellExecuteExW` avec
//!    `SEE_MASK_FLAG_NO_UI` : jamais de boîte d'erreur, et un échec retombe
//!    sur la suite ;
//! 3. sinon, un navigateur installé à son emplacement habituel (Firefox,
//!    Chrome, Edge), lancé directement avec l'adresse ;
//! 4. sinon, une fenêtre de Tune dit en clair que le serveur tourne et quelle
//!    adresse ouvrir.
//!
//! Les décisions sont des fonctions pures, éprouvées sur toutes les machines ;
//! seuls la lecture du registre (en UTF-16, par l'API), le shell et le
//! lancement sont propres à
//! Windows. L'appel est bloquant : `bootstrap` le passe en `spawn_blocking`.

use std::path::{Path, PathBuf};

/// Ce que l'ouverture fera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ouverture {
    /// L'association `http` mène à un programme présent : `start` l'emploie.
    ParLAssociation,
    /// Pas d'association utilisable : ce navigateur installé, directement.
    Navigateur(PathBuf),
    /// Rien à lancer : une fenêtre de Tune donne l'adresse.
    Avis,
}

/// Ce que le shell sait de l'association `http`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Association {
    /// `UserChoice\ProgId`, s'il est posé.
    pub prog_id: Option<String>,
    /// La clé `HKCR\<ProgId>` existe.
    pub prog_id_enregistre: bool,
    /// L'exécutable résolu : celui du `ProgId` s'il y en a un, sinon celui
    /// du protocole `http`.
    pub executable: Option<PathBuf>,
}

/// Rassemble l'association, sans effet de bord.
///
/// `cle_existe(prog_id)` : `HKCR\<prog_id>` existe ; `executable_de(nom,
/// protocole)` : `AssocQueryStringW(ASSOCSTR_EXECUTABLE)` pour ce `ProgId`
/// (`protocole = false`) ou pour le protocole `http` (`true`). Un `ProgId`
/// posé n'est JAMAIS complété par l'exécutable de `http`.
pub(crate) fn lire_association(
    prog_id: Option<String>,
    cle_existe: impl Fn(&str) -> bool,
    executable_de: impl Fn(&str, bool) -> Option<PathBuf>,
) -> Association {
    match prog_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => Association {
            executable: executable_de(&id, false),
            prog_id_enregistre: cle_existe(&id),
            prog_id: Some(id),
        },
        None => Association {
            prog_id: None,
            prog_id_enregistre: false,
            executable: executable_de("http", true),
        },
    }
}

/// L'association mène-t-elle quelque part ?
///
/// Un exécutable nommé doit exister ; sans exécutable, seul un `ProgId`
/// enregistré compte (application du Store).
pub(crate) fn association_utilisable(a: &Association, existe: impl Fn(&Path) -> bool) -> bool {
    match &a.executable {
        Some(exe) => existe(exe),
        None => a.prog_id.is_some() && a.prog_id_enregistre,
    }
}

/// La décision, sans effet de bord.
///
/// `installes` : les emplacements habituels des navigateurs, dans l'ordre de
/// préférence ; `existe` : le système de fichiers, en paramètre pour les
/// témoins.
pub(crate) fn decider(
    association: &Association,
    installes: &[PathBuf],
    existe: impl Fn(&Path) -> bool,
) -> Ouverture {
    if association_utilisable(association, &existe) {
        return Ouverture::ParLAssociation;
    }
    match installes.iter().find(|p| existe(p)) {
        Some(p) => Ouverture::Navigateur(p.clone()),
        None => Ouverture::Avis,
    }
}

/// Les emplacements habituels des navigateurs, dans l'ordre de préférence.
pub(crate) fn navigateurs_habituels(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    const RELATIFS: &[&str] = &[
        r"Mozilla Firefox\firefox.exe",
        r"Google\Chrome\Application\chrome.exe",
        r"Microsoft\Edge\Application\msedge.exe",
    ];
    let racines: Vec<String> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|v| env(v))
        .filter(|r| !r.is_empty())
        .collect();
    let mut chemins = Vec::new();
    for rel in RELATIFS {
        for racine in &racines {
            chemins.push(Path::new(racine).join(rel));
        }
    }
    chemins
}

/// Le message de la fenêtre d'avis, quand rien ne peut s'ouvrir.
pub(crate) fn message_d_avis(url: &str) -> String {
    format!(
        "Tune Server tourne. Aucun navigateur n'a pu être ouvert automatiquement : \
         ouvrez {url} dans votre navigateur."
    )
}

/// La ligne de commande de la fenêtre d'avis, telle que `cmd` la lit.
///
/// `start` ouvre une NOUVELLE fenêtre de console, visible même depuis le
/// serveur détaché (`CREATE_NO_WINDOW`) ; `cmd /K` la garde ouverte. Les
/// caractères que `cmd` interprète sont échappés par `^`.
pub(crate) fn ligne_de_l_avis(url: &str) -> String {
    let mut echappe = String::new();
    for c in message_d_avis(url).chars() {
        if matches!(c, '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '"') {
            echappe.push('^');
        }
        echappe.push(c);
    }
    format!("/C start \"Tune Server\" cmd /K \"chcp 65001 >nul & echo {echappe}\"")
}

#[cfg(windows)]
mod systeme {
    use super::*;
    use std::os::windows::process::CommandExt;
    use tune_core::windows::Win32::Foundation::ERROR_SUCCESS;
    use tune_core::windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use tune_core::windows::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey,
        RegGetValueW, RegOpenKeyExW,
    };
    use tune_core::windows::Win32::UI::Shell::{
        ASSOCF_IS_PROTOCOL, ASSOCF_NONE, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use tune_core::windows::core::{HSTRING, PCWSTR, PWSTR};

    const CREATE_NO_WINDOW: u32 = crate::lancement_sans_console::CREATE_NO_WINDOW;

    /// `HKCU\…\UrlAssociations\http\UserChoice`, valeur `ProgId`, lue en
    /// UTF-16 par l'API (aucune page de code en jeu).
    fn prog_id_choisi() -> Option<String> {
        let cle = HSTRING::from(
            r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\http\UserChoice",
        );
        let valeur = HSTRING::from("ProgId");
        let mut tampon = vec![0u16; 512];
        let mut octets = (tampon.len() * 2) as u32;
        // SAFETY: le tampon vit le temps de l'appel ; `octets` dit sa taille.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &cle,
                &valeur,
                RRF_RT_REG_SZ,
                None,
                Some(tampon.as_mut_ptr().cast()),
                Some(&mut octets),
            )
        };
        if r != ERROR_SUCCESS {
            return None;
        }
        let n = (octets as usize / 2).min(tampon.len());
        let fin = tampon[..n].iter().position(|c| *c == 0).unwrap_or(n);
        let id = String::from_utf16_lossy(&tampon[..fin]);
        (!id.trim().is_empty()).then_some(id)
    }

    fn cle_existe(prog_id: &str) -> bool {
        let cle = HSTRING::from(prog_id);
        let mut h = HKEY::default();
        // SAFETY: `h` reçoit la clé ouverte, refermée aussitôt.
        let r = unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, &cle, None, KEY_READ, &mut h) };
        if r == ERROR_SUCCESS {
            // SAFETY: clé ouverte juste au-dessus.
            let _ = unsafe { RegCloseKey(h) };
            true
        } else {
            false
        }
    }

    fn executable_de(nom: &str, protocole: bool) -> Option<PathBuf> {
        let drapeaux = if protocole {
            ASSOCF_IS_PROTOCOL
        } else {
            ASSOCF_NONE
        };
        let nom = HSTRING::from(nom);
        let verbe = HSTRING::from("open");
        let mut tampon = vec![0u16; 1024];
        let mut longueur = tampon.len() as u32;
        // SAFETY: le tampon vit le temps de l'appel et `longueur` dit sa
        // contenance en caractères, terminateur compris.
        let resultat = unsafe {
            AssocQueryStringW(
                drapeaux,
                ASSOCSTR_EXECUTABLE,
                &nom,
                &verbe,
                Some(PWSTR(tampon.as_mut_ptr())),
                &mut longueur,
            )
        };
        if resultat.is_err() {
            return None;
        }
        let fin = tampon.iter().position(|c| *c == 0).unwrap_or(tampon.len());
        let chemin = String::from_utf16_lossy(&tampon[..fin]);
        let chemin = chemin.trim();
        (!chemin.is_empty()).then(|| PathBuf::from(chemin))
    }

    /// `ShellExecuteExW`, sans aucune interface d'erreur du shell.
    fn ouvrir_par_le_shell(url: &str) -> tune_core::windows::core::Result<()> {
        // SAFETY: initialisation COM du fil courant, recommandée avant
        // `ShellExecuteExW` ; un échec (déjà initialisé autrement) est sans
        // conséquence pour une URL.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        let verbe = HSTRING::from("open");
        let fichier = HSTRING::from(url);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verbe.as_ptr()),
            lpFile: PCWSTR(fichier.as_ptr()),
            // SW_SHOWNORMAL
            nShow: 1,
            ..Default::default()
        };
        // SAFETY: `info` et les chaînes qu'il désigne vivent le temps de
        // l'appel.
        unsafe { ShellExecuteExW(&mut info) }
    }

    fn env(nom: &str) -> Option<String> {
        std::env::var(nom).ok()
    }

    fn avis(url: &str) {
        if let Err(e) = std::process::Command::new("cmd")
            .raw_arg(ligne_de_l_avis(url))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            tracing::warn!(error = %e, "ouverture_du_navigateur_windows_avis_impossible");
        }
    }

    pub(super) fn ouvrir(url: &str) {
        let installes = navigateurs_habituels(env);
        let association = lire_association(prog_id_choisi(), cle_existe, executable_de);
        let mut ouverture = decider(&association, &installes, |p| p.is_file());
        tracing::info!(?ouverture, ?association, "ouverture_du_navigateur_windows");
        if ouverture == Ouverture::ParLAssociation {
            match ouvrir_par_le_shell(url) {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "ouverture_du_navigateur_windows_par_le_shell_echouee — repli"
                    );
                    ouverture = decider(&Association::default(), &installes, |p| p.is_file());
                }
            }
        }
        match ouverture {
            Ouverture::Navigateur(exe) => {
                if let Err(e) = std::process::Command::new(&exe).arg(url).spawn() {
                    tracing::warn!(error = %e, exe = %exe.display(), "ouverture_du_navigateur_windows_echouee");
                    avis(url);
                }
            }
            Ouverture::Avis | Ouverture::ParLAssociation => avis(url),
        }
    }
}

/// Ouvre `url` sous Windows, sans jamais laisser la fenêtre d'erreur du shell
/// pour seul retour. Bloquant : registre, shell, lancement.
#[cfg(windows)]
pub(crate) fn ouvrir(url: &str) {
    systeme::ouvrir(url);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_de(paires: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = paires
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn assoc(exe: Option<&str>) -> Association {
        Association {
            prog_id: None,
            prog_id_enregistre: false,
            executable: exe.map(PathBuf::from),
        }
    }

    /// Le cas du testeur : aucune association utilisable, Firefox installé.
    /// Avant, `start` partait quand même et Windows affichait « Application
    /// introuvable ».
    #[test]
    fn sans_association_firefox_installe_est_lance_directement_9061() {
        let env = env_de(&[
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
        ]);
        let installes = navigateurs_habituels(env);
        let firefox = PathBuf::from(r"C:\Program Files").join(r"Mozilla Firefox\firefox.exe");
        let present = firefox.clone();
        assert_eq!(
            decider(&assoc(None), &installes, |p| p == present.as_path()),
            Ouverture::Navigateur(firefox)
        );
    }

    /// Une association qui nomme un programme désinstallé ne vaut rien :
    /// c'est exactement ce que Windows appelle « Application introuvable ».
    #[test]
    fn une_association_vers_un_programme_absent_ne_compte_pas_9061() {
        let edge = PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe");
        let present = edge.clone();
        let a = assoc(Some(r"C:\Program Files\Ancien\browser.exe"));
        assert_eq!(
            decider(&a, std::slice::from_ref(&edge), |p| p == present.as_path()),
            Ouverture::Navigateur(edge)
        );
    }

    /// Le choix cassé de l'utilisateur (`ProgId` sans commande ni clé) n'est
    /// PAS complété par `HKCR\http` : iexplore y est souvent encore nommé,
    /// alors que le shell suit le choix cassé et rend la même erreur.
    #[test]
    fn un_prog_id_casse_ne_retombe_pas_sur_l_executable_de_http_9061() {
        let iexplore = PathBuf::from(r"C:\Program Files\Internet Explorer\iexplore.exe");
        let vu = std::cell::RefCell::new(Vec::new());
        let a = lire_association(
            Some("FirefoxURL-308046B0AF4A39CB".into()),
            |_| false,
            |nom, protocole| {
                vu.borrow_mut().push((nom.to_string(), protocole));
                (nom == "http" && protocole).then(|| iexplore.clone())
            },
        );
        assert_eq!(a.executable, None, "{a:?}");
        assert_eq!(
            *vu.borrow(),
            vec![("FirefoxURL-308046B0AF4A39CB".to_string(), false)],
            "seul le ProgId choisi est interrogé"
        );
        assert!(!association_utilisable(&a, |_| true));
    }

    /// Une application du Store (ou l'ancien Edge) : pas d'exécutable, mais
    /// la clé `HKCR\<ProgId>` existe — l'association est valide.
    #[test]
    fn un_prog_id_du_store_enregistre_est_valide() {
        let a = lire_association(
            Some("AppXq0fevzme2pys62n3e0fbqa7peapykr8v".into()),
            |_| true,
            |_, _| None,
        );
        assert!(association_utilisable(&a, |_| false));
        assert_eq!(decider(&a, &[], |_| false), Ouverture::ParLAssociation);
    }

    /// Sans choix de l'utilisateur, c'est le protocole `http` qui est lu.
    #[test]
    fn sans_prog_id_le_protocole_http_est_lu() {
        let a = lire_association(
            None,
            |_| true,
            |nom, protocole| (nom == "http" && protocole).then(|| PathBuf::from(r"C:\b\nav.exe")),
        );
        assert_eq!(a.executable, Some(PathBuf::from(r"C:\b\nav.exe")));
        assert!(association_utilisable(&a, |_| true));
        assert!(!association_utilisable(&a, |_| false));
        let vide = lire_association(Some("  ".into()), |_| true, |_, _| None);
        assert_eq!(vide.prog_id, None, "un ProgId vide ne compte pas");
    }

    #[test]
    fn une_association_valide_garde_le_navigateur_choisi_par_l_utilisateur() {
        let firefox = PathBuf::from(r"C:\Program Files\Mozilla Firefox\firefox.exe");
        let a = assoc(Some(r"C:\Program Files\BraveSoftware\brave.exe"));
        assert_eq!(
            decider(&a, &[firefox], |_| true),
            Ouverture::ParLAssociation
        );
    }

    #[test]
    fn rien_a_lancer_donne_l_avis_et_jamais_start_9061() {
        let firefox = PathBuf::from(r"C:\Program Files\Mozilla Firefox\firefox.exe");
        assert_eq!(
            decider(&assoc(None), &[firefox], |_| false),
            Ouverture::Avis
        );
        assert_eq!(decider(&assoc(None), &[], |_| true), Ouverture::Avis);
    }

    #[test]
    fn firefox_puis_chrome_puis_edge() {
        let env = env_de(&[("ProgramFiles", r"C:\PF")]);
        let l = navigateurs_habituels(env);
        let noms: Vec<String> = l
            .iter()
            .map(|p| p.to_string_lossy().replace('/', "\\"))
            .collect();
        assert_eq!(noms.len(), 3, "{noms:?}");
        assert!(noms[0].ends_with(r"firefox.exe"), "{noms:?}");
        assert!(noms[1].ends_with(r"chrome.exe"), "{noms:?}");
        assert!(noms[2].ends_with(r"msedge.exe"), "{noms:?}");
    }

    #[test]
    fn l_avis_donne_l_adresse_et_echappe_ce_que_cmd_interprete() {
        let url = "http://localhost:8888";
        assert!(message_d_avis(url).contains(url));
        let ligne = ligne_de_l_avis(url);
        assert!(
            ligne.starts_with("/C start \"Tune Server\" cmd /K "),
            "{ligne}"
        );
        assert!(ligne.contains("echo Tune Server tourne."), "{ligne}");
        assert!(ligne.contains(url), "{ligne}");
        // Une seule esperluette non échappée : celle qui enchaîne chcp et echo.
        let nues = ligne
            .match_indices('&')
            .filter(|(i, _)| *i == 0 || &ligne[i - 1..*i] != "^")
            .count();
        assert_eq!(nues, 1, "{ligne}");
        let avec_paren = ligne_de_l_avis("http://h/(x)");
        assert!(avec_paren.contains("^(x^)"), "{avec_paren}");
    }
}
