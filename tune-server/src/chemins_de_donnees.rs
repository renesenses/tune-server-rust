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

/// D'où vient le cache de pochettes retenu au démarrage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrigineDuCache {
    /// Le défaut historique ABSOLU de la plateforme (macOS, Windows) :
    /// inchangé.
    DefautDeLaPlateforme,
    /// Le cache imposé par la configuration d'un appareil Tune OS (#5513).
    ImposeParLaConfiguration,
    /// L'ancien `artwork_cache` RELATIF au répertoire courant, gardé parce
    /// qu'il a des images ou que l'installation existait déjà.
    AncienRelatifAuDossierCourant,
    /// `<dossier de données>/artwork_cache` : installation neuve, ou ancien
    /// emplacement qui coïncide avec lui.
    SousLesDonnees,
}

impl OrigineDuCache {
    pub fn id(self) -> &'static str {
        match self {
            Self::DefautDeLaPlateforme => "platform_default",
            Self::ImposeParLaConfiguration => "appliance_configuration",
            Self::AncienRelatifAuDossierCourant => "legacy_working_directory",
            Self::SousLesDonnees => "data_dir",
        }
    }
}

/// « Neuves seulement » (décision de Bertrand, rc14) — le cache par défaut
/// quand la plateforme n'en donne qu'un chemin RELATIF (Linux, et macOS ou
/// Windows privés de `HOME` / `LOCALAPPDATA`).
///
/// Avant, ce relatif se résolvait contre le répertoire courant du processus,
/// sans lien avec `TUNE_DATA_DIR` ni la base : relancé d'ailleurs, le serveur
/// ne trouvait plus aucune pochette. `albums.cover_path` porte un CONDENSAT
/// (#1444), pas un chemin : l'image n'est retrouvée QUE si le fichier est
/// dans le cache servi. On ne déplace donc rien de ce qui existe :
///
/// 1. un défaut déjà absolu (macOS, Windows) : inchangé ;
/// 2. l'ancien emplacement (`cwd/artwork_cache`) est celui du dossier de
///    données (le .deb : `WorkingDirectory=/var/lib/tune`, base relative) :
///    c'est le même dossier, rendu absolu ;
/// 3. l'ancien emplacement a des images : il est gardé tel quel ;
/// 4. le cache sous les données EXISTE, même vide : le choix « neuf » a
///    déjà été fait (il crée le dossier, voir [`preparer_le_cache_retenu`]).
///    Installation neuve redémarrée avant sa première pochette, ou relancée
///    depuis un autre dossier : il est suivi ;
/// 5. l'installation existait (base présente, ou PostgreSQL) : l'ancien
///    emplacement est gardé, comme avant — ses pochettes sont peut-être dans
///    le dossier d'où elle se lance d'habitude, et y revenir doit les rendre ;
/// 6. sinon, installation neuve : `<données>/artwork_cache`.
pub fn cache_de_pochettes_sans_variable(
    par_defaut: &Path,
    cwd: &Path,
    donnees: &Path,
    installation_existante: bool,
    etat: impl Fn(&Path) -> EtatDuDossier,
) -> (PathBuf, OrigineDuCache) {
    if par_defaut.is_absolute() {
        return (
            par_defaut.to_path_buf(),
            OrigineDuCache::DefautDeLaPlateforme,
        );
    }
    let ancien = cwd.join(par_defaut);
    let neuf = donnees.join("artwork_cache");
    if ancien == neuf {
        return (neuf, OrigineDuCache::SousLesDonnees);
    }
    if etat(&ancien) == EtatDuDossier::Garni {
        return (ancien, OrigineDuCache::AncienRelatifAuDossierCourant);
    }
    if etat(&neuf) != EtatDuDossier::Absent {
        return (neuf, OrigineDuCache::SousLesDonnees);
    }
    if installation_existante {
        return (ancien, OrigineDuCache::AncienRelatifAuDossierCourant);
    }
    (neuf, OrigineDuCache::SousLesDonnees)
}

/// L'installation existait-elle avant ce démarrage ? Base SQLite présente
/// sur le disque, ou base PostgreSQL (dont on ne peut rien affirmer : on la
/// tient pour existante). Lu AVANT l'ouverture de la base, qui la créerait.
pub fn installation_existante(db_path: &str, postgres: bool, cwd: &Path) -> bool {
    if postgres {
        return true;
    }
    if db_path.is_empty() || db_path == ":memory:" {
        return false;
    }
    let base = Path::new(db_path);
    if base.is_absolute() {
        base.exists()
    } else {
        cwd.join(base).exists()
    }
}

/// Un défaut du cache de pochettes que l'utilisateur doit VOIR : l'écran
/// « État du serveur » (sonde `/system/health/monitor`), `/system/diagnostics`
/// et le rapport de bogue le portent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvertissementDuCache {
    /// `artwork_cache_unwritable` ou `artwork_cache_relative_to_working_dir`.
    pub code: &'static str,
    pub message: String,
}

pub const CACHE_INSCRIPTIBLE_IMPOSSIBLE: &str = "artwork_cache_unwritable";
pub const CACHE_RELATIF_AU_DOSSIER_COURANT: &str = "artwork_cache_relative_to_working_dir";

/// L'avertissement à montrer pour le cache retenu, s'il y en a un :
/// l'impossibilité d'écrire d'abord (aucune pochette ne sera extraite), puis
/// le cache qui dépend du dossier de lancement. Pas pour un appareil Tune OS,
/// dont l'unité fixe `WorkingDirectory=/opt/tune` (#5596 : ses images y sont).
pub fn avertissement_du_cache(
    dossier: &Path,
    origine: OrigineDuCache,
    appareil: bool,
    ecriture: Result<(), String>,
) -> Option<AvertissementDuCache> {
    if let Err(erreur) = ecriture {
        return Some(AvertissementDuCache {
            code: CACHE_INSCRIPTIBLE_IMPOSSIBLE,
            message: format!(
                "Le cache des pochettes ({}) ne peut pas être créé ou écrit : {erreur}. \
                 Aucune pochette ne sera extraite. Donnez à Tune le droit d'écrire dans ce \
                 dossier, ou posez TUNE_ARTWORK_DIR vers un dossier inscriptible.",
                dossier.display()
            ),
        });
    }
    if origine == OrigineDuCache::AncienRelatifAuDossierCourant && !appareil {
        return Some(AvertissementDuCache {
            code: CACHE_RELATIF_AU_DOSSIER_COURANT,
            message: format!(
                "Le cache des pochettes ({}) dépend du dossier d'où Tune est lancé : lancé \
                 depuis un autre dossier, les pochettes ne s'affichent plus. Posez \
                 TUNE_ARTWORK_DIR={} pour le fixer.",
                dossier.display(),
                dossier.display()
            ),
        });
    }
    None
}

/// Le cache peut-il recevoir des images ? Sans rien créer de durable : s'il
/// existe, un fichier sonde y est écrit puis retiré ; sinon, le plus proche
/// parent existant doit être un dossier où l'on peut écrire (sonde aussi).
/// Ne crée PAS le dossier : un volume externe absent ne doit pas se
/// retrouver avec un dossier fantôme sur le disque système.
pub fn sonder_le_cache(dossier: &Path) -> Result<(), String> {
    let mut cible = dossier;
    while !cible.exists() {
        match cible.parent() {
            Some(p) if !p.as_os_str().is_empty() => cible = p,
            _ => return Err("aucun dossier parent".into()),
        }
    }
    if !cible.is_dir() {
        return Err(format!("{} n'est pas un dossier", cible.display()));
    }
    tempfile::Builder::new()
        .prefix(".sonde-pochettes-")
        .tempfile_in(cible)
        .map(drop)
        .map_err(|e| e.to_string())
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
    /// Le cache retenu au démarrage, hors `TUNE_ARTWORK_DIR`.
    cache: PathBuf,
    origine: OrigineDuCache,
    appareil: bool,
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
    let (cache, origine) = match choix.and_then(|c| c.impose) {
        Some(impose) => (impose, OrigineDuCache::ImposeParLaConfiguration),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let donnees = crate::gel_executeur::dossier_de_donnees(
                std::env::var("TUNE_DATA_DIR").ok().as_deref(),
                &config.db_path,
                &cwd,
            );
            cache_de_pochettes_sans_variable(
                &cache_de_pochettes_par_defaut(
                    Plateforme::courante(),
                    std::env::var_os("HOME").as_deref(),
                    std::env::var("LOCALAPPDATA").ok().as_deref(),
                ),
                &cwd,
                &donnees,
                installation_existante(&config.db_path, config.database_url.is_some(), &cwd),
                etat_du_dossier,
            )
        }
    };
    tracing::info!(
        dossier = %cache.display(),
        origine = origine.id(),
        variable = std::env::var_os("TUNE_ARTWORK_DIR").is_some(),
        "artwork_cache_retenu"
    );
    let retenus = Retenus {
        cache,
        origine,
        appareil,
        base: config.db_path.clone(),
    };
    let _ = RETENUS.set(retenus);
}

/// Après l'attente du disque de données (`wait_for_data_volume`) : crée le
/// cache « sous les données » retenu (le choix est alors durable, même
/// avant la première pochette), puis sonde l'écriture du cache servi et
/// établit l'avertissement. Sonder plus tôt, sur un Tune OS dont le disque
/// externe se monte en retard, testerait un point de montage vide.
pub fn preparer_le_cache_retenu() {
    let _ = AVERTISSEMENT.get_or_init(etablir_l_avertissement);
}

/// Crée `dossier` quand c'est un choix « sous les données » ; ne touche à
/// rien d'autre.
pub fn creer_le_cache_choisi(dossier: &Path, origine: OrigineDuCache) -> std::io::Result<()> {
    if origine == OrigineDuCache::SousLesDonnees {
        std::fs::create_dir_all(dossier)?;
    }
    Ok(())
}

fn etablir_l_avertissement() -> Option<AvertissementDuCache> {
    let r = RETENUS.get()?;
    let variable = std::env::var_os("TUNE_ARTWORK_DIR");
    // `TUNE_ARTWORK_DIR` tranche (Docker) : c'est lui qu'on sonde alors,
    // et posée par l'exploitant elle n'est jamais « relative au dossier
    // courant » au sens de l'avertissement.
    let (servi, origine) = match &variable {
        Some(v) => (PathBuf::from(v), OrigineDuCache::DefautDeLaPlateforme),
        None => {
            if let Err(e) = creer_le_cache_choisi(&r.cache, r.origine) {
                tracing::warn!(dossier = %r.cache.display(), error = %e, "artwork_cache_creation_echouee");
            }
            (r.cache.clone(), r.origine)
        }
    };
    let avertissement =
        avertissement_du_cache(&servi, origine, r.appareil, sonder_le_cache(&servi));
    if let Some(a) = &avertissement {
        tracing::warn!(dossier = %servi.display(), code = a.code, "{}", a.message);
    }
    avertissement
}

static AVERTISSEMENT: OnceLock<Option<AvertissementDuCache>> = OnceLock::new();

/// Le cache retenu au démarrage, hors `TUNE_ARTWORK_DIR` (`None` avant
/// `retenir`, et dans l'hôte mobile qui ne l'appelle pas).
pub(crate) fn cache_retenu() -> Option<&'static Path> {
    RETENUS.get().map(|r| r.cache.as_path())
}

/// D'où vient le cache retenu (`None` avant `retenir`).
pub fn origine_du_cache_retenu() -> Option<OrigineDuCache> {
    RETENUS.get().map(|r| r.origine)
}

/// L'avertissement du cache de pochettes, s'il y en a un. Établi par
/// [`preparer_le_cache_retenu`] ; à défaut (aucun démarrage), au premier
/// appel.
pub fn avertissement_du_cache_retenu() -> Option<&'static AvertissementDuCache> {
    RETENUS.get()?;
    AVERTISSEMENT.get_or_init(etablir_l_avertissement).as_ref()
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

    // ── rc14 : « neuves seulement » ─────────────────────────────────────

    /// La règle « neuves seulement » sur une sonde simulée : `garnis` dit
    /// quels dossiers ont des images.
    fn neuves(
        cwd: &str,
        donnees: &str,
        existante: bool,
        garnis: &[&str],
    ) -> (PathBuf, OrigineDuCache) {
        let garnis: Vec<PathBuf> = garnis.iter().map(PathBuf::from).collect();
        cache_de_pochettes_sans_variable(
            Path::new("artwork_cache"),
            Path::new(cwd),
            Path::new(donnees),
            existante,
            |p| {
                if garnis.iter().any(|g| g == p) {
                    EtatDuDossier::Garni
                } else {
                    EtatDuDossier::Absent
                }
            },
        )
    }

    /// Installation neuve, sans variable : le cache va sous le dossier de
    /// données, plus sous le répertoire courant.
    #[test]
    fn installation_neuve_sans_variable_cache_sous_les_donnees() {
        assert_eq!(
            neuves("/home/a", "/srv/tune", false, &[]),
            (
                PathBuf::from("/srv/tune/artwork_cache"),
                OrigineDuCache::SousLesDonnees
            )
        );
    }

    /// Installation neuve relancée depuis un autre dossier : base présente,
    /// rien dans le nouveau répertoire courant, les images déjà sous les
    /// données — c'est là qu'on les sert (le défaut (a) de l'analyse).
    #[test]
    fn installation_neuve_relancee_d_ailleurs_retrouve_ses_pochettes() {
        assert_eq!(
            neuves(
                "/tmp/ailleurs",
                "/srv/tune",
                true,
                &["/srv/tune/artwork_cache"]
            ),
            (
                PathBuf::from("/srv/tune/artwork_cache"),
                OrigineDuCache::SousLesDonnees
            )
        );
    }

    /// Un ancien cache garni est gardé tel quel, installation existante ou
    /// non, même si le dossier de données a aussi des images.
    #[test]
    fn ancien_cache_present_garde() {
        for existante in [false, true] {
            for garnis in [
                &["/home/a/artwork_cache"][..],
                &["/home/a/artwork_cache", "/srv/tune/artwork_cache"][..],
            ] {
                assert_eq!(
                    neuves("/home/a", "/srv/tune", existante, garnis),
                    (
                        PathBuf::from("/home/a/artwork_cache"),
                        OrigineDuCache::AncienRelatifAuDossierCourant
                    ),
                    "existante {existante}, garnis {garnis:?}"
                );
            }
        }
    }

    /// Installation existante dont le cache relatif est vide (lancée depuis
    /// un autre dossier que d'habitude) : l'ancien emplacement est gardé,
    /// comme avant. Le serveur n'est pas « fixé » sous les données, et
    /// revenir au dossier habituel rend les images.
    #[test]
    fn installation_existante_cache_relatif_vide_garde_l_ancien_emplacement() {
        assert_eq!(
            neuves("/tmp/ailleurs", "/srv/tune", true, &[]),
            (
                PathBuf::from("/tmp/ailleurs/artwork_cache"),
                OrigineDuCache::AncienRelatifAuDossierCourant
            )
        );
    }

    /// .deb (`WorkingDirectory=/var/lib/tune`, base relative) et archive
    /// lancée là où est sa base : l'ancien emplacement EST celui des données.
    /// Même dossier qu'avant, rendu absolu, sans avertissement.
    #[test]
    fn deb_meme_dossier_qu_avant() {
        for existante in [false, true] {
            let (dossier, origine) = neuves("/var/lib/tune", "/var/lib/tune", existante, &[]);
            assert_eq!(dossier, PathBuf::from("/var/lib/tune/artwork_cache"));
            assert_eq!(origine, OrigineDuCache::SousLesDonnees);
            assert_eq!(
                avertissement_du_cache(&dossier, origine, false, Ok(())),
                None
            );
        }
        // `./tune.db` : même dossier, au `.` près.
        assert_eq!(
            neuves("/var/lib/tune", "/var/lib/tune/.", false, &[]).1,
            OrigineDuCache::SousLesDonnees
        );
    }

    /// macOS et Windows : leur défaut est déjà absolu, rien ne change.
    #[test]
    fn defaut_absolu_de_la_plateforme_inchange() {
        for par_defaut in [
            "/Users/a/Library/Application Support/Tune/artwork_cache",
            "C:\\Users\\a\\AppData\\Local\\TuneServer\\artwork_cache",
        ] {
            let par_defaut = PathBuf::from(par_defaut);
            if !par_defaut.is_absolute() {
                continue; // un chemin Windows n'est pas absolu sous Unix
            }
            let (dossier, origine) = cache_de_pochettes_sans_variable(
                &par_defaut,
                Path::new("/"),
                Path::new("/ailleurs"),
                false,
                |_| EtatDuDossier::Absent,
            );
            assert_eq!(
                (dossier, origine),
                (par_defaut.clone(), OrigineDuCache::DefautDeLaPlateforme)
            );
        }
    }

    /// `TUNE_ARTWORK_DIR` posée : respectée, devant le cache retenu.
    #[test]
    fn variable_tune_artwork_dir_respectee() {
        let retenu = PathBuf::from("/srv/tune/artwork_cache");
        assert_eq!(
            cache_de_pochettes(Some("/data/artwork_cache"), Some(&retenu), || {
                panic!("pas de défaut quand la variable est posée")
            }),
            PathBuf::from("/data/artwork_cache")
        );
        assert_eq!(
            cache_de_pochettes(None, Some(&retenu), || panic!("le retenu suffit")),
            retenu
        );
    }

    /// Dossier impossible à créer : la sonde échoue et l'avertissement le
    /// dit, quelle que soit l'origine (un appareil Tune OS compris).
    #[test]
    fn dossier_impossible_a_creer_avertit() {
        let racine = tempfile::tempdir().unwrap();
        let fichier = racine.path().join("pas-un-dossier");
        std::fs::write(&fichier, b"x").unwrap();
        let cache = fichier.join("artwork_cache");
        let sonde = sonder_le_cache(&cache);
        assert!(sonde.is_err(), "{sonde:?}");
        // Rien n'a été créé.
        assert!(!cache.exists());
        for (origine, appareil) in [
            (OrigineDuCache::SousLesDonnees, false),
            (OrigineDuCache::AncienRelatifAuDossierCourant, true),
        ] {
            let a = avertissement_du_cache(&cache, origine, appareil, sonde.clone()).unwrap();
            assert_eq!(a.code, CACHE_INSCRIPTIBLE_IMPOSSIBLE);
            assert!(a.message.contains("TUNE_ARTWORK_DIR"), "{}", a.message);
        }
        // Un dossier inscriptible, absent ou présent : la sonde passe sans
        // laisser de trace.
        let absent = racine.path().join("donnees/artwork_cache");
        assert_eq!(sonder_le_cache(&absent), Ok(()));
        assert!(!absent.exists());
        assert_eq!(sonder_le_cache(racine.path()), Ok(()));
        assert_eq!(std::fs::read_dir(racine.path()).unwrap().count(), 1);
    }

    /// Le cache qui dépend du dossier de lancement avertit — sauf sur un
    /// appareil Tune OS, dont l'unité fixe le répertoire courant (#5596).
    #[test]
    fn cache_relatif_au_dossier_courant_avertit_hors_appareil() {
        let d = Path::new("/home/a/artwork_cache");
        let a = avertissement_du_cache(
            d,
            OrigineDuCache::AncienRelatifAuDossierCourant,
            false,
            Ok(()),
        )
        .unwrap();
        assert_eq!(a.code, CACHE_RELATIF_AU_DOSSIER_COURANT);
        assert!(a.message.contains("TUNE_ARTWORK_DIR=/home/a/artwork_cache"));
        assert_eq!(
            avertissement_du_cache(
                d,
                OrigineDuCache::AncienRelatifAuDossierCourant,
                true,
                Ok(())
            ),
            None
        );
        for origine in [
            OrigineDuCache::SousLesDonnees,
            OrigineDuCache::DefautDeLaPlateforme,
            OrigineDuCache::ImposeParLaConfiguration,
        ] {
            assert_eq!(avertissement_du_cache(d, origine, false, Ok(())), None);
        }
    }

    /// Témoin sur le disque : une installation existante (base SQLite
    /// présente, `TUNE_DB_PATH` absolu) dont les pochettes sont dans
    /// `<dossier habituel>/artwork_cache`. Le serveur les sert depuis là ;
    /// lancé une fois d'ailleurs, il ne se fixe pas sous les données ; de
    /// retour au dossier habituel, chaque condensat de la base est retrouvé.
    #[test]
    fn temoin_une_installation_existante_retrouve_ses_pochettes() {
        let racine = tempfile::tempdir().unwrap();
        let (habituel, ailleurs, donnees) = (
            racine.path().join("habituel"),
            racine.path().join("ailleurs"),
            racine.path().join("donnees"),
        );
        for d in [&habituel, &ailleurs, &donnees] {
            std::fs::create_dir_all(d).unwrap();
        }
        let base = donnees.join("tune.db");
        std::fs::write(&base, b"SQLite format 3\0").unwrap();
        // Ce que la base référence : des condensats (#1444), pas des chemins.
        let condensats = [
            "0123456789abcdef0123456789abcdef",
            "fedcba9876543210fedcba9876543210",
        ];
        std::fs::create_dir_all(habituel.join("artwork_cache")).unwrap();
        for c in condensats {
            std::fs::write(habituel.join(format!("artwork_cache/{c}.jpg")), b"jpg").unwrap();
        }
        let base = base.to_str().unwrap();
        let retenu = |cwd: &Path| {
            let donnees = crate::gel_executeur::dossier_de_donnees(None, base, cwd);
            cache_de_pochettes_sans_variable(
                Path::new("artwork_cache"),
                cwd,
                &donnees,
                installation_existante(base, false, cwd),
                etat_du_dossier,
            )
        };
        let trouvees = |dossier: &Path| {
            condensats
                .iter()
                .filter(|c| dossier.join(format!("{c}.jpg")).is_file())
                .count()
        };

        let (dossier, origine) = retenu(&habituel);
        assert_eq!(dossier, habituel.join("artwork_cache"));
        assert_eq!(origine, OrigineDuCache::AncienRelatifAuDossierCourant);
        assert_eq!(trouvees(&dossier), condensats.len());

        let (dossier, _) = retenu(&ailleurs);
        assert_eq!(dossier, ailleurs.join("artwork_cache"));
        assert_ne!(dossier, donnees.join("artwork_cache"));

        let (dossier, _) = retenu(&habituel);
        assert_eq!(trouvees(&dossier), condensats.len());

        // Contre-épreuve : sans base (installation neuve), le même lancement
        // d'ailleurs part sous les données.
        std::fs::remove_file(base).unwrap();
        assert_eq!(retenu(&ailleurs).0, donnees.join("artwork_cache"));
    }

    /// M1 — installation NEUVE (base hors du dossier courant) arrêtée
    /// avant sa première pochette : au redémarrage, la base existe et le
    /// cache sous les données est encore vide. Il reste choisi, puisque le
    /// premier démarrage l'a créé.
    #[test]
    fn installation_neuve_relancee_avant_sa_premiere_pochette_reste_sous_les_donnees() {
        let racine = tempfile::tempdir().unwrap();
        let (cwd, donnees) = (
            racine.path().join("lance-ici"),
            racine.path().join("donnees"),
        );
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&donnees).unwrap();
        let base = donnees.join("tune.db");
        let base = base.to_str().unwrap();
        let demarrer = || {
            let donnees = crate::gel_executeur::dossier_de_donnees(None, base, &cwd);
            let (dossier, origine) = cache_de_pochettes_sans_variable(
                Path::new("artwork_cache"),
                &cwd,
                &donnees,
                installation_existante(base, false, &cwd),
                etat_du_dossier,
            );
            creer_le_cache_choisi(&dossier, origine).unwrap();
            (dossier, origine)
        };
        let attendu = (
            donnees.join("artwork_cache"),
            OrigineDuCache::SousLesDonnees,
        );
        // 1er démarrage : installation neuve ; l'ouverture crée la base.
        assert_eq!(demarrer(), attendu);
        std::fs::write(base, b"SQLite format 3\0").unwrap();
        assert_eq!(etat_du_dossier(&attendu.0), EtatDuDossier::Vide);
        // Arrêt, puis 2e et 3e démarrages : toujours sous les données.
        assert_eq!(demarrer(), attendu);
        assert_eq!(demarrer(), attendu);
        assert!(!cwd.join("artwork_cache").exists());
    }

    /// Seul le choix « sous les données » crée son dossier.
    #[test]
    fn seul_le_choix_sous_les_donnees_cree_son_dossier() {
        let racine = tempfile::tempdir().unwrap();
        for origine in [
            OrigineDuCache::AncienRelatifAuDossierCourant,
            OrigineDuCache::DefautDeLaPlateforme,
            OrigineDuCache::ImposeParLaConfiguration,
        ] {
            let d = racine.path().join(origine.id());
            creer_le_cache_choisi(&d, origine).unwrap();
            assert!(!d.exists(), "{origine:?}");
        }
    }

    /// L'installation existe si sa base est sur le disque ; PostgreSQL est
    /// tenu pour existant.
    #[test]
    fn installation_existante_selon_la_base() {
        let racine = tempfile::tempdir().unwrap();
        assert!(!installation_existante("tune.db", false, racine.path()));
        std::fs::write(racine.path().join("tune.db"), b"").unwrap();
        assert!(installation_existante("tune.db", false, racine.path()));
        let absolue = racine.path().join("tune.db");
        assert!(installation_existante(
            absolue.to_str().unwrap(),
            false,
            Path::new("/")
        ));
        assert!(installation_existante("absente.db", true, racine.path()));
        assert!(!installation_existante(":memory:", false, racine.path()));
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
