//! #5513 — les chemins de données que le démarrage a retenus, pour les
//! fonctions libres qui n'ont pas la configuration sous la main :
//! [`crate::routes::library::artwork_cache_dir`] et
//! [`crate::routes::system::scan::chemin_du_rapport_de_scan`].
//!
//! Deux défauts corrigés, et RIEN d'autre ne change de place :
//!
//! * **Tune OS après « Déplacer le stockage »** : `appliance_storage` copie le
//!   cache de pochettes sur le volume de données et écrit son chemin ABSOLU
//!   dans `tune.toml` (`artwork_dir`). `artwork_cache_dir()` n'écoutait que
//!   `TUNE_ARTWORK_DIR` : il continuait de servir et de remplir
//!   `/opt/tune/artwork_cache`, la copie externe restant figée. Sur un
//!   appareil Tune OS, un `artwork_dir` absolu de la configuration est donc
//!   désormais suivi. Partout ailleurs — .deb, Docker, archive Linux, macOS,
//!   Windows — le chemin reste celui d'avant, même quand `tune.toml` pose un
//!   `artwork_dir` (décision de Bertrand, 30/09/2026 : ne déplacer le cache
//!   d'aucune autre installation).
//! * **Le rapport de scan** se dérivait de `TUNE_DB_PATH` ou du littéral
//!   `"tune.db"`, jamais de `config.db_path` résolu : sous le LaunchAgent
//!   macOS (répertoire courant `/`) il visait `/tune-scan-report.json`. Il se
//!   dérive maintenant de la base réellement ouverte. Le rapport n'est pas
//!   une donnée de l'utilisateur : il est réécrit à chaque scan.
//!
//! Les règles sont des fonctions PURES, sans `cfg` : la plateforme leur est
//! passée, pour que le cas macOS et le cas Windows s'éprouvent sur Shrek.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::TuneConfig;

/// La plateforme dont on applique le défaut historique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plateforme {
    /// Linux, BSD… : tout ce qui n'est ni macOS ni Windows.
    Autre,
    MacOs,
    Windows,
}

impl Plateforme {
    /// Celle pour laquelle le binaire est compilé.
    pub const fn courante() -> Self {
        if cfg!(target_os = "windows") {
            Plateforme::Windows
        } else if cfg!(target_os = "macos") {
            Plateforme::MacOs
        } else {
            Plateforme::Autre
        }
    }
}

/// Le cache de pochettes par défaut, exactement comme le calculait
/// `artwork_cache_dir()` avant #5513 — recopié à l'identique, `cfg` compris,
/// sous forme pure :
///
/// * Windows : `%LOCALAPPDATA%\TuneServer\artwork_cache`, ou
///   `TuneServer\artwork_cache` sans `LOCALAPPDATA` ;
/// * macOS : `$HOME/Library/Application Support/Tune/artwork_cache`, ou
///   `artwork_cache` sans `HOME` ;
/// * ailleurs : `artwork_cache`, relatif au répertoire courant (le .deb pose
///   `WorkingDirectory=/var/lib/tune`, Tune OS `/opt/tune`).
pub fn cache_de_pochettes_par_defaut(
    plateforme: Plateforme,
    home: Option<&OsStr>,
    local_app_data: Option<&str>,
) -> PathBuf {
    match plateforme {
        Plateforme::Windows => {
            let data_dir = local_app_data
                .map(|d| format!("{d}\\TuneServer"))
                .unwrap_or_else(|| "TuneServer".into());
            PathBuf::from(format!("{data_dir}\\artwork_cache"))
        }
        Plateforme::MacOs => match home {
            Some(home) => {
                PathBuf::from(home).join("Library/Application Support/Tune/artwork_cache")
            }
            None => PathBuf::from("artwork_cache"),
        },
        Plateforme::Autre => PathBuf::from("artwork_cache"),
    }
}

/// Le cache que la configuration impose, s'il y en a un : seulement sur un
/// appareil Tune OS, et seulement pour un chemin ABSOLU — la forme
/// qu'écrit « Déplacer le stockage ». Sur l'appareil non déplacé,
/// `artwork_dir` vaut le relatif `artwork_cache`, identique au défaut.
pub fn cache_de_pochettes_impose(appareil: bool, artwork_dir: &str) -> Option<PathBuf> {
    (appareil && Path::new(artwork_dir).is_absolute()).then(|| PathBuf::from(artwork_dir))
}

/// La règle complète du cache de pochettes (hors isolement des tests) :
/// `TUNE_ARTWORK_DIR` d'abord, comme avant ; puis le cache imposé par la
/// configuration d'un appareil déplacé ; sinon le défaut historique.
pub fn cache_de_pochettes(
    variable: Option<&str>,
    impose: Option<&Path>,
    par_defaut: impl FnOnce() -> PathBuf,
) -> PathBuf {
    if let Some(v) = variable {
        return PathBuf::from(v);
    }
    if let Some(dossier) = impose {
        return dossier.to_path_buf();
    }
    par_defaut()
}

/// Le rapport de scan : à côté de la base retenue au démarrage quand elle
/// est connue ; sinon la formule d'avant (`TUNE_DB_PATH` ou `"tune.db"`).
pub fn rapport_de_scan(base_retenue: Option<&str>, variable: Option<&str>) -> String {
    base_retenue
        .or(variable)
        .unwrap_or("tune.db")
        .replace(".db", "-scan-report.json")
}

struct Retenus {
    cache_impose: Option<PathBuf>,
    base: String,
}

static RETENUS: OnceLock<Retenus> = OnceLock::new();

/// Retient, une fois pour le processus, ce que la configuration chargée dit
/// des chemins. Appelé par le démarrage juste après `TuneConfig::load()`.
pub fn retenir(config: &TuneConfig) {
    let appareil = crate::routes::appliance::is_appliance();
    let retenus = Retenus {
        cache_impose: cache_de_pochettes_impose(appareil, &config.artwork_dir),
        base: config.db_path.clone(),
    };
    if let Some(dossier) = &retenus.cache_impose {
        tracing::info!(path = %dossier.display(), "artwork_dir_depuis_la_configuration_de_l_appareil");
    }
    let _ = RETENUS.set(retenus);
}

/// Le cache imposé retenu au démarrage (`None` avant, ou hors appareil).
pub(crate) fn cache_impose_retenu() -> Option<&'static Path> {
    RETENUS.get().and_then(|r| r.cache_impose.as_deref())
}

/// La base retenue au démarrage (`None` avant `retenir`).
pub(crate) fn base_retenue() -> Option<&'static str> {
    RETENUS.get().map(|r| r.base.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La règle entière, telle que l'appelle `artwork_cache_dir()`.
    fn cache(
        plateforme: Plateforme,
        variable: Option<&str>,
        appareil: bool,
        artwork_dir: &str,
        home: Option<&str>,
        local_app_data: Option<&str>,
    ) -> PathBuf {
        let impose = cache_de_pochettes_impose(appareil, artwork_dir);
        cache_de_pochettes(variable, impose.as_deref(), || {
            cache_de_pochettes_par_defaut(plateforme, home.map(OsStr::new), local_app_data)
        })
    }

    /// .deb : pas de variable, `WorkingDirectory=/var/lib/tune`, `artwork_dir`
    /// par défaut — le relatif `artwork_cache`, comme avant. Et un
    /// `artwork_dir` posé dans `tune.toml` ne le déplace PAS.
    #[test]
    fn deb_garde_son_cache() {
        for artwork_dir in ["artwork_cache", "/srv/tune/pochettes"] {
            assert_eq!(
                cache(Plateforme::Autre, None, false, artwork_dir, None, None),
                PathBuf::from("artwork_cache"),
                "artwork_dir = {artwork_dir}"
            );
        }
    }

    /// Docker : `TUNE_ARTWORK_DIR=/data/artwork_cache` (Dockerfile,
    /// Dockerfile.dist), qui tranche toujours.
    #[test]
    fn docker_garde_son_cache() {
        assert_eq!(
            cache(
                Plateforme::Autre,
                Some("/data/artwork_cache"),
                false,
                "/data/artwork_cache",
                None,
                None
            ),
            PathBuf::from("/data/artwork_cache")
        );
    }

    /// macOS : `Application Support`, que la configuration porte le chemin
    /// résolu par `plan_base_macos` (base relative), le relatif
    /// `artwork_cache` (base absolue) ou un autre dossier.
    #[test]
    fn macos_garde_son_cache() {
        let attendu = PathBuf::from("/Users/a/Library/Application Support/Tune/artwork_cache");
        for artwork_dir in [
            "/Users/a/Library/Application Support/Tune/artwork_cache",
            "artwork_cache",
            "/Volumes/Musique/pochettes",
        ] {
            assert_eq!(
                cache(
                    Plateforme::MacOs,
                    None,
                    false,
                    artwork_dir,
                    Some("/Users/a"),
                    None
                ),
                attendu,
                "artwork_dir = {artwork_dir}"
            );
        }
        assert_eq!(
            cache(Plateforme::MacOs, None, false, "artwork_cache", None, None),
            PathBuf::from("artwork_cache")
        );
    }

    /// Windows : `%LOCALAPPDATA%\TuneServer\artwork_cache`, quelle que soit la
    /// configuration.
    #[test]
    fn windows_garde_son_cache() {
        for artwork_dir in [
            "C:\\Users\\a\\AppData\\Local\\TuneServer\\artwork_cache",
            "artwork_cache",
        ] {
            assert_eq!(
                cache(
                    Plateforme::Windows,
                    None,
                    false,
                    artwork_dir,
                    None,
                    Some("C:\\Users\\a\\AppData\\Local")
                ),
                PathBuf::from("C:\\Users\\a\\AppData\\Local\\TuneServer\\artwork_cache"),
                "artwork_dir = {artwork_dir}"
            );
        }
        assert_eq!(
            cache(
                Plateforme::Windows,
                None,
                false,
                "artwork_cache",
                None,
                None
            ),
            PathBuf::from("TuneServer\\artwork_cache")
        );
    }

    /// Tune OS non déplacé : `WorkingDirectory=/opt/tune`, `artwork_dir`
    /// relatif — le même `artwork_cache` qu'avant.
    #[test]
    fn tune_os_non_deplace_garde_son_cache() {
        assert_eq!(
            cache(Plateforme::Autre, None, true, "artwork_cache", None, None),
            PathBuf::from("artwork_cache")
        );
    }

    /// Le défaut de #5513 : après « Déplacer le stockage », le cache est celui
    /// que `tune.toml` désigne, là où il a été copié.
    #[test]
    fn tune_os_deplace_suit_la_configuration() {
        let deplace = "/mnt/tune-data/TuneData/artwork_cache";
        assert_eq!(
            cache(Plateforme::Autre, None, true, deplace, None, None),
            PathBuf::from(deplace)
        );
        // `TUNE_ARTWORK_DIR` garde la priorité qu'il avait.
        assert_eq!(
            cache(
                Plateforme::Autre,
                Some("/ailleurs"),
                true,
                deplace,
                None,
                None
            ),
            PathBuf::from("/ailleurs")
        );
    }

    /// Le rapport de scan suit la base retenue ; sans elle, l'ancienne
    /// formule. .deb, Docker et Tune OS non déplacé : le même fichier qu'avant.
    #[test]
    fn rapport_de_scan_a_cote_de_la_base_retenue() {
        // macOS (LaunchAgent) : à côté de la base d'Application Support, et
        // plus `/tune-scan-report.json`.
        assert_eq!(
            rapport_de_scan(
                Some("/Users/a/Library/Application Support/Tune/tune.db"),
                None
            ),
            "/Users/a/Library/Application Support/Tune/tune-scan-report.json"
        );
        // .deb / Tune OS non déplacé : relatif, inchangé.
        assert_eq!(
            rapport_de_scan(Some("tune.db"), None),
            "tune-scan-report.json"
        );
        assert_eq!(rapport_de_scan(None, None), "tune-scan-report.json");
        // Docker : `TUNE_DB_PATH=/data/tune.db` est aussi la base retenue.
        assert_eq!(
            rapport_de_scan(Some("/data/tune.db"), Some("/data/tune.db")),
            "/data/tune-scan-report.json"
        );
        assert_eq!(
            rapport_de_scan(None, Some("/data/tune_v2.db")),
            "/data/tune_v2-scan-report.json"
        );
    }
}
