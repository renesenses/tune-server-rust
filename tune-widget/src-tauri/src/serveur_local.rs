//! Option « Démarrer le serveur Tune s'il ne tourne pas » (forum, fil 2868).
//!
//! Au lancement du widget, si le serveur configuré est sur CETTE machine et ne
//! répond pas, le widget lance le serveur Tune installé localement. L'option
//! est désactivée par défaut (`demarrer_serveur_local` dans `config.json`).
//!
//! # Ce que le widget lance, et comment
//!
//! Rien n'est construit à partir d'une saisie : ni l'adresse du serveur ni
//! aucun autre réglage n'entre dans la commande. Le programme lancé est cherché
//! à des emplacements fixes, ou détectés à partir de ce que l'installeur Tune a
//! lui-même écrit ; les arguments sont des constantes. Pas d'interpréteur de
//! commandes, pas d'élévation de droits.
//!
//! - **Windows** : l'installeur NSIS de Tune (`release.yml`) installe par
//!   utilisateur, dans `%LOCALAPPDATA%\Programs\Tune Server` par défaut (le
//!   dossier reste modifiable sur sa page « Dossier »). Le dossier réel se lit
//!   dans la clé de désinstallation qu'il écrit,
//!   `HKCU\…\Uninstall\TuneServer\UninstallString` (`"$INSTDIR\uninstall.exe"`).
//!   La commande est celle du démarrage avec Windows (#8861) et du raccourci du
//!   Bureau : `tune-server.exe --sans-console` (#6253), qui se relance détaché,
//!   sans fenêtre, puis rend la main.
//! - **macOS** : `Tune Server.app`, dans `/Applications` ou `~/Applications`,
//!   ouvert par `/usr/bin/open -g` (en arrière-plan). Son lanceur reste parent
//!   du serveur (#5141) : c'est lui qui obtient l'accès au réseau local.
//!   `TUNE_RELANCE_APRES_MAJ=1` lui demande de ne pas ouvrir d'onglet de
//!   navigateur (le widget est l'interface qu'on vient d'ouvrir).
//! - **Linux** : non pris en charge. Le paquet installe un service système,
//!   que seul `root` démarre ; le widget n'élève pas ses droits.

use std::path::PathBuf;

/// La clé de `config.json`.
pub const CLE_CONFIG: &str = "demarrer_serveur_local";

/// Le nom du programme du serveur sous Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub const EXE_WINDOWS: &str = "tune-server.exe";

/// Le dossier d'installation par défaut, sous `%LOCALAPPDATA%`.
#[cfg_attr(not(windows), allow(dead_code))]
pub const DOSSIER_WINDOWS_PAR_DEFAUT: &str = r"Programs\Tune Server";

/// Le paquet macOS du serveur.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const APP_MACOS: &str = "Tune Server.app";

/// Le serveur configuré est-il sur cette machine ?
///
/// Seules les adresses de bouclage comptent : `localhost`, `127.0.0.0/8`,
/// `::1`. Une adresse du réseau local, même celle de cette machine, désigne
/// pour le widget un autre poste possible : on ne lance rien pour elle. Une
/// adresse illisible n'est pas locale.
pub fn adresse_locale(serveur: &str) -> bool {
    let Ok(url) = url::Url::parse(serveur.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Le dossier d'installation, tiré de la valeur `UninstallString` écrite par
/// l'installeur (`"C:\…\Tune Server\uninstall.exe"`, guillemets compris ou non).
///
/// Ne rend un dossier que si la valeur désigne bien `uninstall.exe` par un
/// chemin absolu (lettre de lecteur ou chemin UNC) : toute autre forme est
/// ignorée, et l'emplacement par défaut prend le relais.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn dossier_depuis_desinstalleur(valeur: &str) -> Option<String> {
    let v = valeur.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v);
    let (dossier, fichier) = v.rsplit_once('\\')?;
    if !fichier.eq_ignore_ascii_case("uninstall.exe") {
        return None;
    }
    let absolu = dossier.starts_with(r"\\")
        || (dossier.len() >= 2
            && dossier.as_bytes()[0].is_ascii_alphabetic()
            && dossier.as_bytes()[1] == b':');
    if !absolu || dossier.contains('"') {
        return None;
    }
    Some(dossier.to_string())
}

/// Les emplacements où chercher `tune-server.exe`, dans l'ordre : le dossier
/// que l'installeur a déclaré, puis le dossier par défaut.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn candidats_windows(desinstalleur: Option<&str>, local_app_data: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dossier) = desinstalleur.and_then(dossier_depuis_desinstalleur) {
        out.push(format!("{dossier}\\{EXE_WINDOWS}"));
    }
    if let Some(base) = local_app_data
        .map(|b| b.trim_end_matches('\\'))
        .filter(|b| !b.is_empty())
    {
        let defaut = format!("{base}\\{DOSSIER_WINDOWS_PAR_DEFAUT}\\{EXE_WINDOWS}");
        if !out.iter().any(|c| c.eq_ignore_ascii_case(&defaut)) {
            out.push(defaut);
        }
    }
    out
}

/// Les emplacements où chercher `Tune Server.app` sous macOS.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn candidats_macos(home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/Applications").join(APP_MACOS)];
    if let Some(h) = home {
        out.push(h.join("Applications").join(APP_MACOS));
    }
    out
}

/// Lance le serveur installé sur cette machine. `Err` dit pourquoi rien n'a
/// été lancé (rien d'installé, système non pris en charge, échec du lancement).
pub fn lancer() -> Result<String, String> {
    lancer_systeme()
}

#[cfg(windows)]
fn valeur_desinstalleur() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\TuneServer")
        .ok()?
        .get_value::<String, _>("UninstallString")
        .ok()
}

#[cfg(windows)]
fn lancer_systeme() -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    /// Pas de fenêtre de console, même un instant.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let local = std::env::var("LOCALAPPDATA").ok();
    let exe = candidats_windows(valeur_desinstalleur().as_deref(), local.as_deref())
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "tune-server.exe introuvable (Tune Server n'est pas installé pour cet utilisateur)"
                .to_string()
        })?;
    let dossier = exe
        .parent()
        .ok_or_else(|| "dossier d'installation illisible".to_string())?;
    Command::new(&exe)
        .arg("--sans-console")
        .current_dir(dossier)
        // Le serveur n'ouvre pas de navigateur : le widget EST l'interface.
        .env_remove("TUNE_OPEN_BROWSER")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| exe.display().to_string())
        .map_err(|e| format!("{}: {e}", exe.display()))
}

#[cfg(target_os = "macos")]
fn lancer_systeme() -> Result<String, String> {
    use std::process::{Command, Stdio};
    let app = candidats_macos(dirs::home_dir())
        .into_iter()
        .find(|p| p.is_dir())
        .ok_or_else(|| format!("{APP_MACOS} introuvable dans /Applications ni ~/Applications"))?;
    Command::new("/usr/bin/open")
        .arg("-g")
        .arg("-a")
        .arg(&app)
        .arg("--env")
        .arg("TUNE_RELANCE_APRES_MAJ=1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| app.display().to_string())
        .map_err(|e| format!("{}: {e}", app.display()))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn lancer_systeme() -> Result<String, String> {
    Err("non pris en charge sur ce système (service système : démarrage par root)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seules_les_adresses_de_bouclage_sont_locales() {
        for local in [
            "http://localhost:8888",
            "http://LOCALHOST:8888/",
            "http://127.0.0.1:8888",
            "http://127.0.1.1:9000",
            "https://localhost",
            "http://[::1]:8888",
        ] {
            assert!(adresse_locale(local), "{local}");
        }
        for distant in [
            "http://192.168.1.18:8888",
            "http://10.0.0.2:8888",
            "http://tune.local:8888",
            "http://localhost.evil.example:8888",
            "http://evil-localhost:8888",
            "http://0.0.0.0:8888",
            "http://[::]:8888",
            "file:///C:/tune-server.exe",
            "localhost:8888",
            "pas une adresse",
            "",
        ] {
            assert!(!adresse_locale(distant), "{distant}");
        }
    }

    #[test]
    fn le_dossier_vient_de_la_cle_de_desinstallation() {
        assert_eq!(
            dossier_depuis_desinstalleur(
                r#""C:\Users\a\AppData\Local\Programs\Tune Server\uninstall.exe""#
            ),
            Some(r"C:\Users\a\AppData\Local\Programs\Tune Server".to_string())
        );
        // Sans guillemets (c'est ainsi que l'écrit l'installeur NSIS de Tune).
        assert_eq!(
            dossier_depuis_desinstalleur(r"D:\Musique\Tune\Uninstall.exe"),
            Some(r"D:\Musique\Tune".to_string())
        );
        assert_eq!(
            dossier_depuis_desinstalleur(r"\\nas\apps\Tune\uninstall.exe"),
            Some(r"\\nas\apps\Tune".to_string())
        );
        // Tout ce qui n'est pas « chemin absolu\uninstall.exe » est ignoré.
        for faux in [
            "",
            "uninstall.exe",
            r"Tune\uninstall.exe",
            r"C:\Tune\autre.exe",
            r"C:\Tune\uninstall.exe /S",
            r#"cmd /c calc & "C:\Tune\uninstall.exe""#,
            r#"C:\Tu"ne\uninstall.exe"#,
        ] {
            assert_eq!(dossier_depuis_desinstalleur(faux), None, "{faux}");
        }
    }

    #[test]
    fn les_candidats_windows_sont_le_dossier_declare_puis_le_defaut() {
        assert_eq!(
            candidats_windows(
                Some(r"D:\Tune\uninstall.exe"),
                Some(r"C:\Users\a\AppData\Local")
            ),
            vec![
                r"D:\Tune\tune-server.exe".to_string(),
                r"C:\Users\a\AppData\Local\Programs\Tune Server\tune-server.exe".to_string(),
            ]
        );
        // Installé au dossier par défaut : pas de doublon.
        assert_eq!(
            candidats_windows(
                Some(r"C:\Users\a\AppData\Local\Programs\Tune Server\uninstall.exe"),
                Some(r"C:\Users\a\AppData\Local\")
            ),
            vec![r"C:\Users\a\AppData\Local\Programs\Tune Server\tune-server.exe".to_string()]
        );
        // Clé absente ou illisible : le défaut seul.
        assert_eq!(
            candidats_windows(Some("n'importe quoi"), Some(r"C:\L")),
            vec![r"C:\L\Programs\Tune Server\tune-server.exe".to_string()]
        );
        assert!(candidats_windows(None, None).is_empty());
        assert!(candidats_windows(None, Some("")).is_empty());
        // Chaque candidat finit par le programme du serveur, et rien d'autre.
        for c in candidats_windows(Some(r"E:\X\uninstall.exe"), Some(r"C:\L")) {
            assert!(c.ends_with(r"\tune-server.exe"), "{c}");
        }
    }

    #[test]
    fn les_candidats_macos_sont_les_dossiers_applications() {
        assert_eq!(
            candidats_macos(Some(PathBuf::from("/Users/a"))),
            vec![
                PathBuf::from("/Applications/Tune Server.app"),
                PathBuf::from("/Users/a/Applications/Tune Server.app"),
            ]
        );
        assert_eq!(
            candidats_macos(None),
            vec![PathBuf::from("/Applications/Tune Server.app")]
        );
    }
}
