//! Système PRÉCIS et mode d'installation, pour le battement de télémétrie.
//!
//! Le battement (`POST /api/v1/telemetry/heartbeat`) ne portait que la
//! famille d'OS (`std::env::consts::OS`) et l'architecture. L'espace admin du
//! site veut compter les installations par système précis : « Ubuntu 24.04.1
//! LTS », « Windows 11 23H2 », « macOS 15.4 », Docker ou Tune OS, Raspberry Pi
//! 4 ou 5. Ce module calcule trois champs, UNE fois par processus :
//!
//! - `os_version` : nom et version lisibles du système ;
//! - `install_kind` : `docker`, `tune-os`, `homebrew`, `windows-installer`,
//!   `macos-app`, `ios-app`, `package` (le `.deb`), sinon `other` ;
//! - `device_model` : modèle de la carte ARM sous Linux
//!   (`/proc/device-tree/model`), absent ailleurs.
//!
//! **Rien d'identifiant** : ni nom de machine, ni numéro de série, ni
//! utilisateur, ni chemin. Le chemin de l'exécutable n'est lu que pour en
//! déduire `install_kind` ; il ne quitte jamais la machine. Chaque valeur
//! envoyée est ramenée à l'ASCII imprimable et bornée en longueur
//! ([`nettoyer`]).
//!
//! Toute la logique est faite de fonctions pures sur ce qu'elles lisent, pour
//! se tester sans la machine correspondante ; seule [`detecter`] touche au
//! système de fichiers (et, sous Windows, lit le registre par `reg.exe`, sans
//! PowerShell).

use std::path::Path;
use std::sync::OnceLock;

/// Longueur maximale d'`os_version` et de `device_model` (octets ASCII).
pub const LONGUEUR_MAX: usize = 64;

/// Marqueur posé par les images Tune OS (`image/build-*-image.sh`), le même
/// que lit `tune-server/src/routes/appliance.rs`.
pub const MARQUEUR_TUNE_OS: &str = "/etc/tune-appliance";

/// Liste dpkg du paquet `.deb` (`scripts/build-deb.sh`, `Package: tune-server`).
const LISTE_DPKG: &str = "/var/lib/dpkg/info/tune-server.list";

/// Ce que le battement dit de la plateforme, au-delà de `os` et `arch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plateforme {
    pub os_version: Option<String>,
    pub install_kind: &'static str,
    pub device_model: Option<String>,
}

/// La plateforme de CE processus, calculée au premier appel puis figée.
pub fn plateforme() -> &'static Plateforme {
    static PLATEFORME: OnceLock<Plateforme> = OnceLock::new();
    PLATEFORME.get_or_init(detecter)
}

/// Ramène une valeur lue sur la machine à de l'ASCII imprimable : les blancs
/// et caractères de contrôle deviennent une espace, le reste hors ASCII est
/// retiré, les espaces sont fusionnées, la longueur bornée à `max`. Une valeur
/// vide après nettoyage vaut `None`.
pub fn nettoyer(brut: &str, max: usize) -> Option<String> {
    let mut sortie = String::with_capacity(brut.len().min(max));
    let mut espace_en_attente = false;
    for c in brut.chars() {
        if c.is_whitespace() || c.is_control() {
            espace_en_attente = !sortie.is_empty();
            continue;
        }
        if !(c.is_ascii_graphic()) {
            continue;
        }
        if espace_en_attente {
            if sortie.len() + 1 >= max {
                break;
            }
            sortie.push(' ');
            espace_en_attente = false;
        }
        if sortie.len() >= max {
            break;
        }
        sortie.push(c);
    }
    let sortie = sortie.trim_end().to_string();
    (!sortie.is_empty()).then_some(sortie)
}

/// Une valeur d'`os-release` (`KEY=valeur`, `KEY="valeur"` ou `KEY='valeur'`).
fn valeur_os_release(contenu: &str, cle: &str) -> Option<String> {
    contenu.lines().find_map(|ligne| {
        let (k, v) = ligne.trim().split_once('=')?;
        if k.trim() != cle {
            return None;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        // Échappements shell admis par os-release(5) : \" \\ \$ \`
        let mut s = String::with_capacity(v.len());
        let mut it = v.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                if let Some(n) = it.next() {
                    s.push(n);
                }
            } else {
                s.push(c);
            }
        }
        Some(s)
    })
}

/// Nom lisible d'une distribution Linux à partir de `/etc/os-release` :
/// `PRETTY_NAME`, sinon `NAME VERSION_ID`, sinon `NAME`.
pub fn depuis_os_release(contenu: &str) -> Option<String> {
    if let Some(p) =
        valeur_os_release(contenu, "PRETTY_NAME").and_then(|p| nettoyer(&p, LONGUEUR_MAX))
    {
        return Some(p);
    }
    let nom = valeur_os_release(contenu, "NAME")?;
    let brut = match valeur_os_release(contenu, "VERSION_ID") {
        Some(v) => format!("{nom} {v}"),
        None => nom,
    };
    nettoyer(&brut, LONGUEUR_MAX)
}

/// `ProductVersion` de `/System/Library/CoreServices/SystemVersion.plist`,
/// préfixée du nom donné (`macOS`, `iOS`).
pub fn depuis_system_version_plist(contenu: &str, nom: &str) -> Option<String> {
    let apres = contenu.split_once("<key>ProductVersion</key>")?.1;
    let valeur = apres.split_once("<string>")?.1.split_once("</string>")?.0;
    nettoyer(&format!("{nom} {}", valeur.trim()), LONGUEUR_MAX)
}

/// Version d'Android à partir de `build.prop` (`ro.build.version.release`).
pub fn depuis_build_prop(contenu: &str) -> Option<String> {
    let v = contenu.lines().find_map(|l| {
        l.trim()
            .strip_prefix("ro.build.version.release=")
            .map(str::to_string)
    })?;
    nettoyer(&format!("Android {}", v.trim()), LONGUEUR_MAX)
}

/// Nom de Windows à partir de la sortie de
/// `reg query "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion"`.
///
/// `ProductName` dit encore « Windows 10 » sur Windows 11 : c'est le numéro de
/// build (≥ 22000) qui les distingue. `DisplayVersion` (« 23H2 », ou
/// `ReleaseId` sur les Windows 10 anciens) donne la mise à jour ; à défaut, le
/// build. L'édition (Home, Pro) n'est pas reprise, sauf pour Windows Server
/// où elle fait partie du nom.
pub fn depuis_registre_windows(sortie: &str) -> Option<String> {
    let valeur = |nom: &str| -> Option<String> {
        sortie.lines().find_map(|l| {
            let champs: Vec<&str> = l
                .split("    ")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            match champs.as_slice() {
                [n, t, v, ..] if *n == nom && t.starts_with("REG_") => Some((*v).to_string()),
                _ => None,
            }
        })
    };
    let produit = valeur("ProductName");
    let build: Option<u32> = valeur("CurrentBuildNumber")
        .or_else(|| valeur("CurrentBuild"))
        .and_then(|b| b.parse().ok());
    let famille = match (&produit, build) {
        (Some(p), _) if p.contains("Server") => p.clone(),
        (_, Some(b)) if b >= 22000 => "Windows 11".to_string(),
        (Some(p), _) if p.starts_with("Windows 10") => "Windows 10".to_string(),
        (Some(p), _) => p.clone(),
        (None, Some(_)) => "Windows".to_string(),
        (None, None) => return None,
    };
    let brut = match (
        valeur("DisplayVersion").or_else(|| valeur("ReleaseId")),
        build,
    ) {
        (Some(d), _) => format!("{famille} {d}"),
        (None, Some(b)) => format!("{famille} (build {b})"),
        (None, None) => famille,
    };
    nettoyer(&brut, LONGUEUR_MAX)
}

/// Modèle de la carte, pour Linux ARM seulement : contenu de
/// `/proc/device-tree/model` (terminé par un NUL), nettoyé.
pub fn modele_arm(os: &str, arch: &str, contenu: &str) -> Option<String> {
    if os != "linux" || !matches!(arch, "aarch64" | "arm") {
        return None;
    }
    nettoyer(contenu, LONGUEUR_MAX)
}

/// Ce que la détection du mode d'installation lit sur la machine.
#[derive(Debug, Default, Clone)]
pub struct IndicesInstallation<'a> {
    /// `std::env::consts::OS`.
    pub os: &'a str,
    /// [`MARQUEUR_TUNE_OS`] existe.
    pub marqueur_tune_os: bool,
    /// `/.dockerenv` ou `/run/.containerenv` existe (l'image Docker de Tune,
    /// lancée par Docker ou Podman).
    pub dockerenv: bool,
    /// Contenu de `/proc/1/cgroup`.
    pub cgroup: &'a str,
    /// Chemin de l'exécutable (jamais envoyé).
    pub exe: &'a str,
    /// La liste dpkg du paquet `tune-server` existe.
    pub paquet_dpkg: bool,
    /// `uninstall.exe` est posé à côté de l'exécutable (installateur NSIS).
    pub desinstallateur_voisin: bool,
}

/// Le mode d'installation, du plus spécifique au plus général.
pub fn type_installation(i: &IndicesInstallation<'_>) -> &'static str {
    let exe = i.exe.replace('\\', "/");
    if i.os == "ios" {
        return "ios-app";
    }
    // Tune OS d'abord : l'image peut embarquer le paquet ou un cgroup
    // quelconque, mais c'est le marqueur qui la définit.
    if i.marqueur_tune_os {
        return "tune-os";
    }
    if i.dockerenv || i.cgroup.contains("docker") {
        return "docker";
    }
    if exe.contains("/Cellar/tune-server/") {
        return "homebrew";
    }
    if i.os == "windows" && i.desinstallateur_voisin {
        return "windows-installer";
    }
    if i.os == "macos" && exe.contains(".app/Contents/MacOS/") {
        return "macos-app";
    }
    if i.os == "linux" && i.paquet_dpkg && exe.starts_with("/opt/tune/") {
        return "package";
    }
    "other"
}

fn lire(chemin: &str) -> String {
    std::fs::read_to_string(chemin).unwrap_or_default()
}

#[cfg(windows)]
fn sortie_registre_windows() -> String {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn sortie_registre_windows() -> String {
    String::new()
}

fn os_version_lu(os: &str) -> Option<String> {
    match os {
        "linux" => {
            let c = lire("/etc/os-release");
            let c = if c.is_empty() {
                lire("/usr/lib/os-release")
            } else {
                c
            };
            depuis_os_release(&c).or_else(|| Some("Linux".to_string()))
        }
        "macos" | "ios" => {
            let nom = if os == "macos" { "macOS" } else { "iOS" };
            depuis_system_version_plist(
                &lire("/System/Library/CoreServices/SystemVersion.plist"),
                nom,
            )
            .or_else(|| Some(nom.to_string()))
        }
        "windows" => depuis_registre_windows(&sortie_registre_windows())
            .or_else(|| Some("Windows".to_string())),
        "android" => {
            depuis_build_prop(&lire("/system/build.prop")).or_else(|| Some("Android".to_string()))
        }
        autre => nettoyer(autre, LONGUEUR_MAX),
    }
}

/// Lit la machine. Appelée une seule fois, par [`plateforme`].
pub fn detecter() -> Plateforme {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let exe_chemin = std::env::current_exe().ok();
    let exe = exe_chemin
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let desinstallateur_voisin = exe_chemin
        .as_deref()
        .and_then(Path::parent)
        .is_some_and(|d| d.join("uninstall.exe").exists());
    let cgroup = if os == "linux" {
        lire("/proc/1/cgroup")
    } else {
        String::new()
    };
    let install_kind = type_installation(&IndicesInstallation {
        os,
        marqueur_tune_os: os == "linux" && Path::new(MARQUEUR_TUNE_OS).exists(),
        dockerenv: os == "linux"
            && (Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()),
        cgroup: &cgroup,
        exe: &exe,
        paquet_dpkg: os == "linux" && Path::new(LISTE_DPKG).exists(),
        desinstallateur_voisin,
    });
    let device_model = if os == "linux" {
        modele_arm(os, arch, &lire("/proc/device-tree/model"))
    } else {
        None
    };
    Plateforme {
        os_version: os_version_lu(os),
        install_kind,
        device_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_ubuntu_rend_pretty_name() {
        let c = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(depuis_os_release(c).as_deref(), Some("Ubuntu 24.04.1 LTS"));
    }

    #[test]
    fn os_release_debian_et_fedora() {
        let debian = "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"12\"\n";
        assert_eq!(
            depuis_os_release(debian).as_deref(),
            Some("Debian GNU/Linux 12 (bookworm)")
        );
        let fedora = "NAME='Fedora Linux'\nVERSION_ID=43\nPRETTY_NAME='Fedora Linux 43 (Workstation Edition)'\n";
        assert_eq!(
            depuis_os_release(fedora).as_deref(),
            Some("Fedora Linux 43 (Workstation Edition)")
        );
    }

    /// Contre-épreuve : sans PRETTY_NAME, on retombe sur NAME + VERSION_ID et
    /// non sur une valeur vide ; un fichier vide ne rend rien.
    #[test]
    fn os_release_sans_pretty_name_se_replie_sur_name_version() {
        let c = "NAME=Alpine Linux\nID=alpine\nVERSION_ID=3.20.3\n";
        assert_eq!(depuis_os_release(c).as_deref(), Some("Alpine Linux 3.20.3"));
        assert_eq!(depuis_os_release(""), None);
        // Une clé qui ne fait que COMMENCER par PRETTY_NAME n'est pas lue.
        assert_eq!(depuis_os_release("PRETTY_NAME_X=\"faux\"\n"), None);
    }

    #[test]
    fn os_release_echappements_et_non_ascii_nettoyes() {
        let c = "PRETTY_NAME=\"Mon \\\"OS\\\" é\t 1.0\"\n";
        assert_eq!(depuis_os_release(c).as_deref(), Some("Mon \"OS\" 1.0"));
    }

    #[test]
    fn nettoyer_borne_la_longueur_et_rejette_le_vide() {
        let long = "A".repeat(500);
        assert_eq!(
            nettoyer(&long, LONGUEUR_MAX).map(|s| s.len()),
            Some(LONGUEUR_MAX)
        );
        assert_eq!(nettoyer("\u{0}\u{7}\n  \t", LONGUEUR_MAX), None);
        assert_eq!(
            nettoyer("  a\u{1b}[31m  b  ", LONGUEUR_MAX).as_deref(),
            Some("a [31m b")
        );
        assert!(nettoyer(&"é".repeat(300), LONGUEUR_MAX).is_none());
    }

    #[test]
    fn macos_et_ios_lisent_product_version() {
        let plist = "<dict>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductVersion</key>\n\t<string>15.4</string>\n</dict>";
        assert_eq!(
            depuis_system_version_plist(plist, "macOS").as_deref(),
            Some("macOS 15.4")
        );
        assert_eq!(
            depuis_system_version_plist(plist, "iOS").as_deref(),
            Some("iOS 15.4")
        );
        assert_eq!(depuis_system_version_plist("<dict></dict>", "macOS"), None);
    }

    #[test]
    fn android_lit_build_prop() {
        let c = "ro.build.id=AP2A\nro.build.version.release=14\n";
        assert_eq!(depuis_build_prop(c).as_deref(), Some("Android 14"));
        assert_eq!(depuis_build_prop("ro.build.id=x\n"), None);
    }

    const REG_WIN11: &str = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\r\n    SystemRoot    REG_SZ    C:\\WINDOWS\r\n    CurrentBuildNumber    REG_SZ    22631\r\n    DisplayVersion    REG_SZ    23H2\r\n    ProductName    REG_SZ    Windows 10 Pro\r\n    RegisteredOwner    REG_SZ    Jean Dupont\r\n";

    #[test]
    fn windows_11_se_reconnait_au_build_et_ignore_le_proprietaire() {
        let v = depuis_registre_windows(REG_WIN11).unwrap();
        assert_eq!(v, "Windows 11 23H2");
        assert!(!v.contains("Dupont"), "rien d'identifiant");
    }

    /// Contre-épreuve : un build < 22000 reste Windows 10 ; sans
    /// DisplayVersion, le build est donné ; Windows Server garde son nom.
    #[test]
    fn windows_10_server_et_build_seul() {
        let w10 = "    CurrentBuildNumber    REG_SZ    19045\r\n    DisplayVersion    REG_SZ    22H2\r\n    ProductName    REG_SZ    Windows 10 Home\r\n";
        assert_eq!(
            depuis_registre_windows(w10).as_deref(),
            Some("Windows 10 22H2")
        );
        let ancien = "    CurrentBuildNumber    REG_SZ    17763\r\n    ProductName    REG_SZ    Windows 10 Enterprise LTSC 2019\r\n";
        assert_eq!(
            depuis_registre_windows(ancien).as_deref(),
            Some("Windows 10 (build 17763)")
        );
        let srv = "    CurrentBuildNumber    REG_SZ    20348\r\n    DisplayVersion    REG_SZ    21H2\r\n    ProductName    REG_SZ    Windows Server 2022 Standard\r\n";
        assert_eq!(
            depuis_registre_windows(srv).as_deref(),
            Some("Windows Server 2022 Standard 21H2")
        );
        assert_eq!(depuis_registre_windows(""), None);
    }

    #[test]
    fn modele_raspberry_pi_lu_sur_linux_arm() {
        let c = "Raspberry Pi 5 Model B Rev 1.0\u{0}";
        assert_eq!(
            modele_arm("linux", "aarch64", c).as_deref(),
            Some("Raspberry Pi 5 Model B Rev 1.0")
        );
        assert_eq!(
            modele_arm("linux", "arm", "Allwinner H6\u{0}").as_deref(),
            Some("Allwinner H6")
        );
    }

    /// Contre-épreuve : pas de modèle hors Linux ARM, ni pour un fichier vide.
    #[test]
    fn pas_de_modele_hors_linux_arm() {
        let c = "Raspberry Pi 5 Model B Rev 1.0\u{0}";
        assert_eq!(modele_arm("linux", "x86_64", c), None);
        assert_eq!(modele_arm("macos", "aarch64", c), None);
        assert_eq!(modele_arm("linux", "aarch64", "\u{0}"), None);
    }

    fn linux<'a>() -> IndicesInstallation<'a> {
        IndicesInstallation {
            os: "linux",
            exe: "/home/x/tune-server",
            ..Default::default()
        }
    }

    #[test]
    fn docker_se_reconnait_a_dockerenv_ou_au_cgroup() {
        assert_eq!(
            type_installation(&IndicesInstallation {
                dockerenv: true,
                ..linux()
            }),
            "docker"
        );
        assert_eq!(
            type_installation(&IndicesInstallation {
                cgroup: "0::/system.slice/docker-0123abcd.scope",
                ..linux()
            }),
            "docker"
        );
        // Contre-épreuve : machine ordinaire.
        assert_eq!(
            type_installation(&IndicesInstallation {
                cgroup: "0::/init.scope",
                ..linux()
            }),
            "other"
        );
    }

    #[test]
    fn tune_os_prime_sur_docker_et_le_paquet() {
        let i = IndicesInstallation {
            marqueur_tune_os: true,
            dockerenv: true,
            paquet_dpkg: true,
            exe: "/opt/tune/tune-server",
            ..linux()
        };
        assert_eq!(type_installation(&i), "tune-os");
        // Contre-épreuve : sans le marqueur, c'est Docker.
        assert_eq!(
            type_installation(&IndicesInstallation {
                marqueur_tune_os: false,
                ..i
            }),
            "docker"
        );
    }

    #[test]
    fn paquet_deb_exige_la_liste_dpkg_et_opt_tune() {
        let i = IndicesInstallation {
            paquet_dpkg: true,
            exe: "/opt/tune/tune-server",
            ..linux()
        };
        assert_eq!(type_installation(&i), "package");
        assert_eq!(
            type_installation(&IndicesInstallation {
                paquet_dpkg: false,
                ..i.clone()
            }),
            "other"
        );
        assert_eq!(
            type_installation(&IndicesInstallation {
                exe: "/home/x/tune-server",
                ..i
            }),
            "other"
        );
    }

    #[test]
    fn homebrew_app_macos_installateur_windows_et_ios() {
        let brew = IndicesInstallation {
            os: "macos",
            exe: "/opt/homebrew/Cellar/tune-server/1.0.0/bin/tune-server",
            ..Default::default()
        };
        assert_eq!(type_installation(&brew), "homebrew");
        let app = IndicesInstallation {
            os: "macos",
            exe: "/Applications/Tune.app/Contents/MacOS/Tune",
            ..Default::default()
        };
        assert_eq!(type_installation(&app), "macos-app");
        let nu = IndicesInstallation {
            os: "macos",
            exe: "/usr/local/bin/tune-server",
            ..Default::default()
        };
        assert_eq!(type_installation(&nu), "other");
        let win = IndicesInstallation {
            os: "windows",
            exe: r"C:\Users\x\AppData\Local\Programs\Tune Server\tune-server.exe",
            desinstallateur_voisin: true,
            ..Default::default()
        };
        assert_eq!(type_installation(&win), "windows-installer");
        assert_eq!(
            type_installation(&IndicesInstallation {
                desinstallateur_voisin: false,
                ..win
            }),
            "other"
        );
        assert_eq!(
            type_installation(&IndicesInstallation {
                os: "ios",
                ..Default::default()
            }),
            "ios-app"
        );
    }

    /// La détection réelle rend des valeurs bornées et ASCII, et toujours un
    /// `install_kind` de la liste fermée.
    #[test]
    fn detection_reelle_bornee() {
        let p = detecter();
        for v in [&p.os_version, &p.device_model].into_iter().flatten() {
            assert!(
                v.len() <= LONGUEUR_MAX && v.bytes().all(|b| (0x20..0x7f).contains(&b)),
                "{v:?}"
            );
        }
        assert!(matches!(
            p.install_kind,
            "docker"
                | "tune-os"
                | "homebrew"
                | "windows-installer"
                | "macos-app"
                | "ios-app"
                | "package"
                | "other"
        ));
        assert!(
            std::ptr::eq(plateforme(), plateforme()),
            "calculée une fois"
        );
    }
}
