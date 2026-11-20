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
//! Désormais, l'ouverture regarde d'abord où mène l'association :
//!
//! 1. le choix de l'utilisateur pour `http` (`UserChoice`, `ProgId`), puis la
//!    commande `shell\open\command` de ce `ProgId` (ou, à défaut, celle de
//!    `HKCR\http`) ; si l'exécutable qu'elle nomme EXISTE, `start` l'emploie,
//!    comme avant ;
//! 2. sinon, un navigateur installé à son emplacement habituel (Firefox,
//!    Chrome, Edge), lancé directement avec l'adresse ;
//! 3. sinon, une fenêtre de Tune dit en clair que le serveur tourne et quelle
//!    adresse ouvrir — jamais la fenêtre d'erreur de Windows.
//!
//! Les décisions sont des fonctions pures, éprouvées sur toutes les machines ;
//! seule la lecture du registre et le lancement sont propres à Windows.

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

/// La décision, sans effet de bord.
///
/// `programme_associe` : l'exécutable que nomme la commande d'ouverture de
/// `http` (`None` : pas d'association). `existe` : le système de fichiers, en
/// paramètre pour les témoins. `installes` : les emplacements habituels des
/// navigateurs, dans l'ordre de préférence.
pub(crate) fn decider(
    programme_associe: Option<&Path>,
    installes: &[PathBuf],
    existe: impl Fn(&Path) -> bool,
) -> Ouverture {
    if programme_associe.is_some_and(&existe) {
        return Ouverture::ParLAssociation;
    }
    match installes.iter().find(|p| existe(p)) {
        Some(p) => Ouverture::Navigateur(p.clone()),
        None => Ouverture::Avis,
    }
}

/// La valeur d'une ligne de `reg query` : `    <nom>    REG_SZ    <valeur>`.
///
/// Le nom de la valeur par défaut est traduit (« (Default) », « (par
/// défaut) ») : on ne le lit pas, on prend ce qui suit le type.
pub(crate) fn valeur_de_reg_query(sortie: &str) -> Option<String> {
    for ligne in sortie.lines() {
        for type_ in ["REG_EXPAND_SZ", "REG_SZ"] {
            if let Some((_, reste)) = ligne.split_once(&format!("    {type_}    ")) {
                let v = reste.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Développe les `%VARIABLE%` d'une valeur `REG_EXPAND_SZ`.
pub(crate) fn developper(valeur: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let mut sortie = String::new();
    let mut reste = valeur;
    while let Some(debut) = reste.find('%') {
        sortie.push_str(&reste[..debut]);
        let apres = &reste[debut + 1..];
        match apres.find('%') {
            Some(fin) => {
                let nom = &apres[..fin];
                match env(nom) {
                    Some(v) if !nom.is_empty() => sortie.push_str(&v),
                    _ => {
                        sortie.push('%');
                        sortie.push_str(nom);
                        sortie.push('%');
                    }
                }
                reste = &apres[fin + 1..];
            }
            None => {
                sortie.push('%');
                reste = apres;
            }
        }
    }
    sortie.push_str(reste);
    sortie
}

/// L'exécutable d'une commande d'ouverture :
/// `"C:\Program Files\Mozilla Firefox\firefox.exe" -osint -url "%1"`.
pub(crate) fn executable_de_la_commande(commande: &str) -> Option<PathBuf> {
    let c = commande.trim();
    let exe = if let Some(guillemets) = c.strip_prefix('"') {
        guillemets.split('"').next()?
    } else {
        // Sans guillemets : jusqu'au `.exe`, qui peut suivre des espaces
        // (`C:\Program Files\…\x.exe -arg`), sinon jusqu'au premier blanc.
        let bas = c.to_ascii_lowercase();
        match bas.find(".exe") {
            Some(i) => &c[..i + 4],
            None => c.split_whitespace().next()?,
        }
    };
    let exe = exe.trim();
    (!exe.is_empty()).then(|| PathBuf::from(exe))
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

    const CREATE_NO_WINDOW: u32 = crate::lancement_sans_console::CREATE_NO_WINDOW;

    fn reg_query(cle: &str, valeur: Option<&str>) -> Option<String> {
        let mut cmd = std::process::Command::new("reg");
        cmd.arg("query").arg(cle);
        match valeur {
            Some(v) => {
                cmd.arg("/v").arg(v);
            }
            None => {
                cmd.arg("/ve");
            }
        }
        let sortie = cmd.creation_flags(CREATE_NO_WINDOW).output().ok()?;
        if !sortie.status.success() {
            return None;
        }
        valeur_de_reg_query(&String::from_utf8_lossy(&sortie.stdout))
    }

    fn env(nom: &str) -> Option<String> {
        std::env::var(nom).ok()
    }

    /// L'exécutable vers lequel mène l'association `http`, s'il y en a une.
    pub(super) fn programme_associe() -> Option<PathBuf> {
        let prog_id = reg_query(
            r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\http\UserChoice",
            Some("ProgId"),
        );
        let commande = prog_id
            .and_then(|id| reg_query(&format!(r"HKCR\{id}\shell\open\command"), None))
            .or_else(|| reg_query(r"HKCR\http\shell\open\command", None))?;
        executable_de_la_commande(&developper(&commande, env))
    }

    pub(super) fn ouvrir(url: &str) {
        let installes = navigateurs_habituels(env);
        let associe = programme_associe();
        let ouverture = decider(associe.as_deref(), &installes, |p| p.is_file());
        tracing::info!(
            ?ouverture,
            programme_associe = ?associe,
            "ouverture_du_navigateur_windows"
        );
        let resultat = match &ouverture {
            Ouverture::ParLAssociation => std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn(),
            Ouverture::Navigateur(exe) => std::process::Command::new(exe).arg(url).spawn(),
            Ouverture::Avis => std::process::Command::new("cmd")
                .raw_arg(ligne_de_l_avis(url))
                .spawn(),
        };
        if let Err(e) = resultat {
            tracing::warn!(error = %e, ?ouverture, "ouverture_du_navigateur_windows_echouee");
        }
    }
}

/// Ouvre `url` sous Windows, sans jamais laisser la fenêtre d'erreur du shell
/// pour seul retour.
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
            decider(None, &installes, |p| p == present.as_path()),
            Ouverture::Navigateur(firefox)
        );
    }

    /// Une association qui nomme un programme désinstallé ne vaut rien :
    /// c'est exactement ce que Windows appelle « Application introuvable ».
    #[test]
    fn une_association_vers_un_programme_absent_ne_compte_pas_9061() {
        let mort = PathBuf::from(r"C:\Program Files\Ancien\browser.exe");
        let edge = PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe");
        let present = edge.clone();
        assert_eq!(
            decider(Some(&mort), std::slice::from_ref(&edge), |p| p
                == present.as_path()),
            Ouverture::Navigateur(edge)
        );
    }

    #[test]
    fn une_association_valide_garde_le_navigateur_choisi_par_l_utilisateur() {
        let choisi = PathBuf::from(r"C:\Program Files\BraveSoftware\brave.exe");
        let firefox = PathBuf::from(r"C:\Program Files\Mozilla Firefox\firefox.exe");
        assert_eq!(
            decider(Some(&choisi), &[firefox], |_| true),
            Ouverture::ParLAssociation
        );
    }

    #[test]
    fn rien_a_lancer_donne_l_avis_et_jamais_start_9061() {
        let firefox = PathBuf::from(r"C:\Program Files\Mozilla Firefox\firefox.exe");
        assert_eq!(decider(None, &[firefox], |_| false), Ouverture::Avis);
        assert_eq!(decider(None, &[], |_| true), Ouverture::Avis);
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
    fn lit_la_valeur_de_reg_query_quelle_que_soit_la_langue() {
        let progid = "\r\nHKEY_CURRENT_USER\\Software\\...\\UserChoice\r\n    ProgId    REG_SZ    FirefoxURL-308046B0AF4A39CB\r\n\r\n";
        assert_eq!(
            valeur_de_reg_query(progid).as_deref(),
            Some("FirefoxURL-308046B0AF4A39CB")
        );
        let defaut_fr = "\r\nHKEY_CLASSES_ROOT\\MSEdgeHTM\\shell\\open\\command\r\n    (par défaut)    REG_SZ    \"C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe\" --single-argument %1\r\n";
        assert_eq!(
            valeur_de_reg_query(defaut_fr).as_deref(),
            Some(
                "\"C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe\" --single-argument %1"
            )
        );
        let etendue = "    (Default)    REG_EXPAND_SZ    %ProgramFiles%\\x\\y.exe \"%1\"\r\n";
        assert_eq!(
            valeur_de_reg_query(etendue).as_deref(),
            Some("%ProgramFiles%\\x\\y.exe \"%1\"")
        );
        assert_eq!(valeur_de_reg_query("ERREUR : clé introuvable"), None);
        assert_eq!(valeur_de_reg_query("    (Default)    REG_SZ    \r\n"), None);
    }

    #[test]
    fn l_executable_de_la_commande_avec_ou_sans_guillemets() {
        assert_eq!(
            executable_de_la_commande(
                r#""C:\Program Files\Mozilla Firefox\firefox.exe" -osint -url "%1""#
            ),
            Some(PathBuf::from(
                r"C:\Program Files\Mozilla Firefox\firefox.exe"
            ))
        );
        assert_eq!(
            executable_de_la_commande(r#"C:\Program Files\X\x.EXE "%1""#),
            Some(PathBuf::from(r"C:\Program Files\X\x.EXE"))
        );
        assert_eq!(
            executable_de_la_commande(r"C:\bin\nav %1"),
            Some(PathBuf::from(r"C:\bin\nav"))
        );
        assert_eq!(executable_de_la_commande("  "), None);
        assert_eq!(executable_de_la_commande(r#""""#), None);
    }

    #[test]
    fn developpe_les_variables_connues_et_garde_les_autres() {
        let env = env_de(&[("ProgramFiles", r"C:\Program Files")]);
        assert_eq!(
            developper(r"%ProgramFiles%\x.exe %1 %INCONNUE% 50%", env),
            r"C:\Program Files\x.exe %1 %INCONNUE% 50%"
        );
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
