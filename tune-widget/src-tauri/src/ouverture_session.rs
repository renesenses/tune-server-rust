//! « Ouvrir Tune Client à l'ouverture de session » (#9244, fil 2966).
//!
//! Désactivée par défaut. Avec la case « Démarrer le serveur Tune s'il ne
//! tourne pas » (`serveur_local`), c'est ce qui démarre aussi le SERVEUR avec
//! la session : Tune Client s'ouvre, discrètement (`--session` : pas de
//! fenêtre, seulement l'icône), et lance le serveur local muet.
//!
//! L'état affiché est celui du système, pas un réglage recopié : la case
//! reflète ce qui se passera vraiment à la prochaine ouverture de session.
//!
//! - **macOS** : un agent de lancement de l'utilisateur,
//!   `~/Library/LaunchAgents/fr.mozaiklabs.tune-client.plist`, qui fait
//!   `open -g -b fr.mozaiklabs.tune-widget --args --session` (par
//!   l'identifiant : l'app peut être déplacée). macOS signale l'ajout
//!   (« Éléments en arrière-plan ») et le liste dans Réglages Système ›
//!   Général › Ouverture.
//! - **Windows** : la valeur `Tune Client` de
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, comme la valeur
//!   `Tune Server` que pose #8861 pour le serveur lui-même (noms distincts :
//!   les deux cases sont indépendantes). Une valeur désactivée dans le
//!   Gestionnaire des tâches (`…\Explorer\StartupApproved\Run`) compte comme
//!   décochée ; cocher efface ce « désactivé ».
//! - **Linux** : non pris en charge.

/// L'argument de lancement « ouverture de session » : pas de fenêtre.
pub const ARG_SESSION: &str = "--session";

/// Le système sait-il ouvrir Tune Client avec la session ?
pub const PRIS_EN_CHARGE: bool = cfg!(any(windows, target_os = "macos"));

/// L'étiquette de l'agent de lancement macOS (et le nom de son fichier).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const ETIQUETTE_MACOS: &str = "fr.mozaiklabs.tune-client";

/// L'identifiant du paquet de Tune Client (`tauri.conf.json`, inchangé #9257).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const ID_PAQUET_MACOS: &str = "fr.mozaiklabs.tune-widget";

/// La valeur de la clé `Run` sous Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub const VALEUR_WINDOWS: &str = "Tune Client";

/// Le lancement a-t-il été fait par l'ouverture de session ?
pub fn lance_par_la_session<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == ARG_SESSION)
}

/// Le contenu de l'agent de lancement macOS. Constantes seulement.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn plist_macos() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{ETIQUETTE_MACOS}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/bin/open</string>
		<string>-g</string>
		<string>-b</string>
		<string>{ID_PAQUET_MACOS}</string>
		<string>--args</string>
		<string>{ARG_SESSION}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// La commande de la valeur `Run`, guillemets autour du chemin.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn commande_windows(exe: &str) -> String {
    format!("\"{exe}\" {ARG_SESSION}")
}

/// `StartupApproved\Run` : premier octet impair = désactivé par
/// l'utilisateur dans le Gestionnaire des tâches (`03 00 …`), pair = permis
/// (`02 00 …`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn desactivee_par_le_gestionnaire(octets: &[u8]) -> bool {
    octets.first().is_some_and(|b| b & 1 == 1)
}

#[cfg(target_os = "macos")]
fn fichier_macos() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library")
            .join("LaunchAgents")
            .join(format!("{ETIQUETTE_MACOS}.plist"))
    })
}

/// Tune Client s'ouvrira-t-il à la prochaine ouverture de session ?
#[cfg(target_os = "macos")]
pub fn active() -> bool {
    fichier_macos().is_some_and(|f| f.is_file())
}

#[cfg(target_os = "macos")]
pub fn regler(actif: bool) -> Result<(), String> {
    let f = fichier_macos().ok_or_else(|| "dossier personnel introuvable".to_string())?;
    if actif {
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(&f, plist_macos()).map_err(|e| format!("{}: {e}", f.display()))
    } else {
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {e}", f.display()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
const CLE_RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const CLE_APPROBATION: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[cfg(windows)]
pub fn active() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let presente = hkcu
        .open_subkey(CLE_RUN)
        .and_then(|k| k.get_value::<String, _>(VALEUR_WINDOWS))
        .is_ok();
    let desactivee = hkcu
        .open_subkey(CLE_APPROBATION)
        .and_then(|k| k.get_raw_value(VALEUR_WINDOWS))
        .is_ok_and(|v| desactivee_par_le_gestionnaire(&v.bytes));
    presente && !desactivee
}

#[cfg(windows)]
pub fn regler(actif: bool) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    // Cocher ou décocher efface un « désactivé » du Gestionnaire des tâches.
    if let Ok(k) = hkcu.open_subkey_with_flags(CLE_APPROBATION, KEY_SET_VALUE) {
        let _ = k.delete_value(VALEUR_WINDOWS);
    }
    let (run, _) = hkcu
        .create_subkey(CLE_RUN)
        .map_err(|e| format!("HKCU\\{CLE_RUN}: {e}"))?;
    if actif {
        let exe = std::env::current_exe().map_err(|e| format!("exécutable : {e}"))?;
        run.set_value(
            VALEUR_WINDOWS,
            &commande_windows(&exe.display().to_string()),
        )
        .map_err(|e| format!("HKCU\\{CLE_RUN}\\{VALEUR_WINDOWS}: {e}"))
    } else {
        match run.delete_value(VALEUR_WINDOWS) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("HKCU\\{CLE_RUN}\\{VALEUR_WINDOWS}: {e}"))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn active() -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn regler(_actif: bool) -> Result<(), String> {
    Err("non pris en charge sur ce système".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seul_l_argument_exact_signale_la_session() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(lance_par_la_session(v(&["tune-widget", "--session"])));
        assert!(!lance_par_la_session(v(&["tune-widget"])));
        assert!(!lance_par_la_session(v(&["tune-widget", "--sessions"])));
        assert!(!lance_par_la_session(v(&["tune-widget", "-psn_0_1234"])));
    }

    #[test]
    fn l_agent_macos_ouvre_tune_client_par_son_identifiant_sans_fenetre() {
        let p = plist_macos();
        assert!(p.contains("<string>fr.mozaiklabs.tune-client</string>"));
        let args = "<string>/usr/bin/open</string>\n\t\t<string>-g</string>\n\t\t<string>-b</string>\n\t\t<string>fr.mozaiklabs.tune-widget</string>\n\t\t<string>--args</string>\n\t\t<string>--session</string>";
        assert!(p.contains(args), "{p}");
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        // Pas de KeepAlive : quitter Tune Client doit le laisser quitté.
        assert!(!p.contains("KeepAlive"));
    }

    #[test]
    fn la_commande_windows_protege_le_chemin() {
        assert_eq!(
            commande_windows(r"C:\Users\a\AppData\Local\Tune Client\tune-widget.exe"),
            r#""C:\Users\a\AppData\Local\Tune Client\tune-widget.exe" --session"#
        );
    }

    #[test]
    fn le_gestionnaire_des_taches_desactive_par_un_octet_impair() {
        assert!(!desactivee_par_le_gestionnaire(&[2, 0, 0, 0]));
        assert!(desactivee_par_le_gestionnaire(&[3, 0, 0, 0]));
        assert!(!desactivee_par_le_gestionnaire(&[]));
    }

    #[test]
    fn la_case_n_existe_que_sous_windows_et_macos() {
        assert_eq!(PRIS_EN_CHARGE, cfg!(windows) || cfg!(target_os = "macos"));
    }
}
