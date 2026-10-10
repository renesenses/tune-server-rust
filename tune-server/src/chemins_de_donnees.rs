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
//!   désormais suivi — mais pas la valeur qu'écrit D'ORIGINE l'image
//!   (`/opt/tune/data/artwork_cache`), ni un dossier vide quand l'ancien a
//!   les images : la rc1 le faisait et tous les appareils Tune OS ont perdu
//!   leurs pochettes (#5596, voir [`cache_de_pochettes_impose`]). Partout ailleurs — .deb, Docker, archive Linux, macOS,
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

/// #5596 — la valeur qu'écrivent D'ORIGINE les images Tune OS dans
/// `tune.toml` (`tune-os/modules/14-install-tune-server.sh`, et
/// `image/build-nuc-image.sh`, `image/build-sunxi-image.sh`). Ce n'est PAS la
/// marque d'un stockage déplacé : jusqu'à la 0.9.169, le serveur ignorait
/// cette valeur et rangeait les pochettes dans `/opt/tune/artwork_cache`
/// (relatif au `WorkingDirectory=/opt/tune`). La 1.0.0-rc1 l'a suivie
/// aveuglément, et tous les appareils Tune OS ont perdu leurs images.
pub const CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS: &str = "/opt/tune/data/artwork_cache";

/// Ce que montre un dossier de cache sur le disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtatDuDossier {
    /// Le dossier n'existe pas (ou volume externe absent).
    Absent,
    /// Il existe, sans aucun fichier.
    Vide,
    /// Il contient au moins un fichier.
    Garni,
}

/// La décision du démarrage sur le cache de pochettes d'un appareil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoixDuCache {
    /// Le cache imposé par la configuration ; `None` = défaut historique.
    pub impose: Option<PathBuf>,
    /// Des fichiers à recueillir, sans rien écraser ni effacer :
    /// `(depuis, vers)`. Les images que l'autre dossier est seul à avoir
    /// (rc1, pochettes communautaires, déplacement incomplet) rejoignent
    /// celui qui est servi.
    pub recueil: Option<(PathBuf, PathBuf)>,
    /// Pour le journal.
    pub raison: &'static str,
}

/// Le cache que la configuration impose, s'il y en a un — seulement sur un
/// appareil Tune OS, seulement pour un `artwork_dir` ABSOLU (la forme
/// qu'écrit « Déplacer le stockage »), et JAMAIS au prix des images
/// (#5596) :
///
/// * `artwork_dir` = la valeur d'origine de l'image
///   ([`CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS`]) : ce n'est pas un déplacement.
///   Si le cache historique (`historique`, `/opt/tune/artwork_cache`) a des
///   fichiers, il reste servi, comme en 0.9.169, et ce que le dossier
///   configuré est seul à avoir y est recueilli. Sinon (appareil neuf,
///   installé en rc1), le dossier configuré est suivi.
/// * tout autre chemin absolu (stockage déplacé) : il est suivi (#5513),
///   et ce que le cache historique est seul à avoir y est recueilli — sauf
///   s'il existe mais est VIDE alors que l'historique a des images : on garde
///   l'historique plutôt que de servir un dossier vide.
pub fn cache_de_pochettes_impose(
    appareil: bool,
    artwork_dir: &str,
    historique: &Path,
    etat: impl Fn(&Path) -> EtatDuDossier,
) -> Option<ChoixDuCache> {
    let configure = Path::new(artwork_dir);
    if !appareil || !configure.is_absolute() || configure == historique {
        return None;
    }
    let configure = configure.to_path_buf();
    let (c, h) = (etat(&configure), etat(historique));
    let recueillir = |depuis: &Path, e: EtatDuDossier, vers: &Path| {
        (e == EtatDuDossier::Garni).then(|| (depuis.to_path_buf(), vers.to_path_buf()))
    };
    if configure == Path::new(CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS) {
        if h == EtatDuDossier::Garni {
            return Some(ChoixDuCache {
                impose: None,
                recueil: recueillir(&configure, c, historique),
                raison: "valeur_d_origine_de_l_image_cache_historique_garde",
            });
        }
        return Some(ChoixDuCache {
            impose: Some(configure),
            recueil: None,
            raison: "valeur_d_origine_de_l_image_sans_cache_historique",
        });
    }
    if c == EtatDuDossier::Vide && h == EtatDuDossier::Garni {
        return Some(ChoixDuCache {
            impose: None,
            recueil: None,
            raison: "cache_deplace_vide_cache_historique_garde",
        });
    }
    let recueil = if c == EtatDuDossier::Absent {
        None
    } else {
        recueillir(historique, h, &configure)
    };
    Some(ChoixDuCache {
        impose: Some(configure),
        recueil,
        raison: "stockage_deplace",
    })
}

/// L'état d'un dossier sur le disque : un seul fichier suffit à le dire
/// garni (un sous-dossier vide ne compte pas).
pub fn etat_du_dossier(dossier: &Path) -> EtatDuDossier {
    fn a_un_fichier(dossier: &Path, profondeur: u8) -> bool {
        let Ok(entrees) = std::fs::read_dir(dossier) else {
            return false;
        };
        entrees.flatten().any(|e| match e.file_type() {
            Ok(t) if t.is_file() => true,
            Ok(t) if t.is_dir() && profondeur > 0 => a_un_fichier(&e.path(), profondeur - 1),
            _ => false,
        })
    }
    if !dossier.is_dir() {
        EtatDuDossier::Absent
    } else if a_un_fichier(dossier, 2) {
        EtatDuDossier::Garni
    } else {
        EtatDuDossier::Vide
    }
}

/// Recopie dans `vers` les fichiers de `depuis` qu'il n'a pas encore —
/// lien dur si possible (même volume : instantané, sans place), sinon copie
/// par un temporaire renommé. N'écrase rien, n'efface rien. Rend le nombre
/// de fichiers recueillis.
pub fn recueillir(depuis: &Path, vers: &Path) -> std::io::Result<u64> {
    let mut n = 0;
    std::fs::create_dir_all(vers)?;
    for entree in std::fs::read_dir(depuis)?.flatten() {
        let (source, cible) = (entree.path(), vers.join(entree.file_name()));
        let Ok(type_) = entree.file_type() else {
            continue;
        };
        if type_.is_dir() {
            n += recueillir(&source, &cible).unwrap_or(0);
            continue;
        }
        let nom = entree.file_name();
        if !type_.is_file() || cible.exists() || nom.to_string_lossy().ends_with(".tmp") {
            continue;
        }
        if std::fs::hard_link(&source, &cible).is_ok() {
            n += 1;
            continue;
        }
        let temporaire = vers.join(format!(".{}.recueil-5596.tmp", nom.to_string_lossy()));
        if std::fs::copy(&source, &temporaire).is_ok()
            && std::fs::rename(&temporaire, &cible).is_ok()
        {
            n += 1;
        } else {
            let _ = std::fs::remove_file(&temporaire);
        }
    }
    Ok(n)
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
/// des chemins. Appelé par le démarrage juste après `TuneConfig::load()` et
/// l'installation du journal. Lance, s'il y a lieu, le recueil des images
/// (#5596) dans un fil à part : le démarrage ne l'attend pas.
pub fn retenir(config: &TuneConfig) {
    let appareil = crate::routes::appliance::is_appliance();
    let historique = cache_de_pochettes_par_defaut(
        Plateforme::courante(),
        std::env::var_os("HOME").as_deref(),
        std::env::var("LOCALAPPDATA").ok().as_deref(),
    );
    let historique = if historique.is_relative() {
        std::env::current_dir()
            .map(|d| d.join(&historique))
            .unwrap_or(historique)
    } else {
        historique
    };
    let choix = if std::env::var_os("TUNE_ARTWORK_DIR").is_some() {
        None
    } else {
        cache_de_pochettes_impose(appareil, &config.artwork_dir, &historique, etat_du_dossier)
    };
    if let Some(choix) = &choix {
        tracing::info!(
            configure = %config.artwork_dir,
            historique = %historique.display(),
            suivi = %choix.impose.as_deref().unwrap_or(&historique).display(),
            raison = choix.raison,
            "artwork_dir_de_l_appareil"
        );
        if let Some((depuis, vers)) = choix.recueil.clone() {
            std::thread::spawn(move || match recueillir(&depuis, &vers) {
                Ok(n) => tracing::info!(
                    depuis = %depuis.display(),
                    vers = %vers.display(),
                    recueillis = n,
                    "artwork_recueil_5596_termine"
                ),
                Err(e) => tracing::warn!(
                    depuis = %depuis.display(),
                    vers = %vers.display(),
                    error = %e,
                    "artwork_recueil_5596_echoue"
                ),
            });
        }
    }
    let retenus = Retenus {
        cache_impose: choix.and_then(|c| c.impose),
        base: config.db_path.clone(),
    };
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
        // Les deux dossiers garnis : le cas du stockage réellement déplacé.
        let impose = cache_de_pochettes_impose(
            appareil,
            artwork_dir,
            Path::new("/opt/tune/artwork_cache"),
            |_| EtatDuDossier::Garni,
        )
        .and_then(|c| c.impose);
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

    const HISTORIQUE: &str = "/opt/tune/artwork_cache";
    const DEPLACE: &str = "/srv/tune-data/TuneData/artwork_cache";

    fn choix(
        artwork_dir: &str,
        configure: EtatDuDossier,
        historique: EtatDuDossier,
    ) -> Option<ChoixDuCache> {
        cache_de_pochettes_impose(true, artwork_dir, Path::new(HISTORIQUE), |p| {
            if p == Path::new(HISTORIQUE) {
                historique
            } else {
                configure
            }
        })
    }

    /// #5596 — l'appareil Tune OS d'origine, tel que l'a vu le testeur :
    /// `tune.toml` à la valeur de l'image, les images dans
    /// `/opt/tune/artwork_cache`. Le cache historique reste servi (le
    /// relatif `artwork_cache`, comme en 0.9.169), et ce que la rc1 a pu
    /// écrire dans le dossier configuré y est recueilli.
    #[test]
    fn tune_os_d_origine_garde_ses_images() {
        use EtatDuDossier::*;
        for configure in [Absent, Vide, Garni] {
            let c = choix(CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS, configure, Garni).unwrap();
            assert_eq!(c.impose, None, "configuré {configure:?}");
            assert_eq!(
                c.recueil,
                (configure == Garni).then(|| (
                    PathBuf::from(CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS),
                    PathBuf::from(HISTORIQUE)
                )),
                "configuré {configure:?}"
            );
            let servi = cache_de_pochettes(None, c.impose.as_deref(), || {
                cache_de_pochettes_par_defaut(Plateforme::Autre, None, None)
            });
            assert_eq!(servi, PathBuf::from("artwork_cache"));
        }
    }

    /// Contre-épreuve : la règle de la rc1 (suivre tout chemin absolu)
    /// servait ici `/opt/tune/data/artwork_cache` — le dossier sans images.
    #[test]
    fn contre_epreuve_la_regle_rc1_perdait_les_images() {
        let regle_rc1 = |appareil: bool, artwork_dir: &str| {
            (appareil && Path::new(artwork_dir).is_absolute()).then(|| PathBuf::from(artwork_dir))
        };
        assert_eq!(
            regle_rc1(true, CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS),
            Some(PathBuf::from(CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS))
        );
        assert_ne!(
            choix(
                CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS,
                EtatDuDossier::Vide,
                EtatDuDossier::Garni
            )
            .unwrap()
            .impose,
            regle_rc1(true, CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS)
        );
    }

    /// Appareil neuf installé en rc1 : rien dans l'historique, le dossier
    /// configuré (garni ou non) est suivi — rien à perdre.
    #[test]
    fn tune_os_neuf_suit_la_valeur_de_l_image() {
        use EtatDuDossier::*;
        for configure in [Absent, Vide, Garni] {
            for historique in [Absent, Vide] {
                let c = choix(CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS, configure, historique).unwrap();
                assert_eq!(
                    c.impose,
                    Some(PathBuf::from(CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS))
                );
                assert_eq!(c.recueil, None);
            }
        }
    }

    /// Stockage réellement déplacé : le cache configuré est suivi (#5513), et
    /// ce que l'historique est seul à avoir y est recueilli.
    #[test]
    fn stockage_deplace_suit_le_cache_configure() {
        let c = choix(DEPLACE, EtatDuDossier::Garni, EtatDuDossier::Garni).unwrap();
        assert_eq!(c.impose, Some(PathBuf::from(DEPLACE)));
        assert_eq!(
            c.recueil,
            Some((PathBuf::from(HISTORIQUE), PathBuf::from(DEPLACE)))
        );
        let c = choix(DEPLACE, EtatDuDossier::Garni, EtatDuDossier::Absent).unwrap();
        assert_eq!((c.impose, c.recueil), (Some(PathBuf::from(DEPLACE)), None));
        // Volume absent au démarrage : on suit la configuration, sans recueil.
        let c = choix(DEPLACE, EtatDuDossier::Absent, EtatDuDossier::Garni).unwrap();
        assert_eq!((c.impose, c.recueil), (Some(PathBuf::from(DEPLACE)), None));
    }

    /// Déplacement qui a copié un dossier vide (la source était la valeur
    /// brute de `tune.toml`) : on ne sert pas un cache vide.
    #[test]
    fn stockage_deplace_vide_garde_l_historique() {
        let c = choix(DEPLACE, EtatDuDossier::Vide, EtatDuDossier::Garni).unwrap();
        assert_eq!(c.impose, None);
    }

    /// Hors appareil, ou `artwork_dir` relatif : aucune décision, rien ne
    /// bouge.
    #[test]
    fn hors_appareil_rien_ne_change() {
        let sonde = |_: &Path| panic!("aucune sonde du disque hors appareil");
        for artwork_dir in [CACHE_D_ORIGINE_DE_L_IMAGE_TUNE_OS, DEPLACE, "artwork_cache"] {
            assert_eq!(
                cache_de_pochettes_impose(false, artwork_dir, Path::new(HISTORIQUE), sonde),
                None
            );
        }
        assert_eq!(
            cache_de_pochettes_impose(true, "artwork_cache", Path::new(HISTORIQUE), sonde),
            None
        );
    }

    /// Sur le disque : l'état d'un dossier, et un recueil qui n'écrase rien.
    #[test]
    fn etat_et_recueil_sur_le_disque() {
        let racine = tempfile::tempdir().unwrap();
        let (configure, historique) = (
            racine.path().join("data/artwork_cache"),
            racine.path().join("artwork_cache"),
        );
        assert_eq!(etat_du_dossier(&historique), EtatDuDossier::Absent);
        std::fs::create_dir_all(historique.join("vide")).unwrap();
        assert_eq!(etat_du_dossier(&historique), EtatDuDossier::Vide);
        std::fs::write(historique.join("aa.jpg"), b"historique").unwrap();
        assert_eq!(etat_du_dossier(&historique), EtatDuDossier::Garni);

        std::fs::create_dir_all(configure.join("sous")).unwrap();
        std::fs::write(configure.join("aa.jpg"), b"rc1").unwrap();
        std::fs::write(configure.join("bb.jpg"), b"televerse-en-rc1").unwrap();
        std::fs::write(configure.join("sous/cc.jpg"), b"cc").unwrap();
        std::fs::write(configure.join("x.tmp"), b"tmp").unwrap();

        assert_eq!(recueillir(&configure, &historique).unwrap(), 2);
        assert_eq!(
            std::fs::read(historique.join("aa.jpg")).unwrap(),
            b"historique"
        );
        assert_eq!(
            std::fs::read(historique.join("bb.jpg")).unwrap(),
            b"televerse-en-rc1"
        );
        assert_eq!(
            std::fs::read(historique.join("sous/cc.jpg")).unwrap(),
            b"cc"
        );
        assert!(!historique.join("x.tmp").exists());
        // La source est intacte, et un second passage ne fait rien.
        assert!(configure.join("bb.jpg").exists());
        assert_eq!(recueillir(&configure, &historique).unwrap(), 0);
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
