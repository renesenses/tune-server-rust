//! #5467 — les tests de la lib n'écrivent plus dans l'arbre source.
//!
//! Avant ce module, `cargo test -p tune-server --lib` créait
//! `tune-server/artwork_cache/` et `tune-server/queue_state/` dans le
//! répertoire courant — c'est-à-dire dans la caisse elle-même. Deux causes :
//!
//! * `AppState::new(":memory:", 0, Default::default())` — la forme de plus de
//!   deux cents tests — garde la `TuneConfig` par défaut, dont `db_path`
//!   (`"tune.db"`) et `artwork_dir` (`"artwork_cache"`) sont RELATIFS
//!   (`config.rs`, `impl Default for TuneConfig`). Tout ce qui dérive un chemin
//!   de `state.config.db_path` le résout donc depuis le répertoire courant :
//!   `queue_persistence::queue_dir` (`tune-core`) y pose `queue_state/`.
//! * `routes::library::artwork_cache_dir()`, sans `TUNE_ARTWORK_DIR`, rend le
//!   chemin relatif `artwork_cache` sous Linux (et, sous macOS, le VRAI dossier
//!   `~/Library/Application Support/Tune/artwork_cache` de la personne qui lance
//!   les tests).
//!
//! En build de test SEULEMENT, ces deux chemins sont redirigés vers un
//! répertoire temporaire : un par `AppState` pour la configuration, un par
//! processus pour `artwork_cache_dir()` (fonction libre, sans état).
//!
//! #5512 — « build de test » ne veut plus dire seulement `#[cfg(test)]`. Ce
//! drapeau ne s'allume que pour la lib testée elle-même ; ses tests
//! d'intégration (`tune-server/tests/`) la lient construite SANS lui, et
//! continuaient donc d'écrire `artwork_cache/`, `queue_state/` et
//! `tune-scan-report.json` dans l'arbre. [`actif`] consulte en plus
//! `tune_core::test_scratch::ISOLEMENT_DISQUE_DE_TEST`, que seuls les
//! `[dev-dependencies]` de `tune-server` allument. Le rapport de scan, dont le
//! chemin ne vient pas de la configuration, est isolé par le même module
//! ([`chemin_du_rapport_de_scan`]). Dans le binaire publié, [`actif`] vaut
//! `false` à la compilation et rien de ce module ne s'exécute.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::TuneConfig;

static RACINE: OnceLock<PathBuf> = OnceLock::new();

/// Vrai dans toute construction de TESTS de `tune-server` : ceux de la lib
/// (`cfg(test)`) comme ceux d'intégration (#5512, la feature
/// `isolement-disque-de-test` de `tune-core`, allumée par les
/// `[dev-dependencies]`). Faux, et connu faux dès la compilation, dans le
/// binaire publié.
pub(crate) const fn actif() -> bool {
    cfg!(test) || tune_core::test_scratch::ISOLEMENT_DISQUE_DE_TEST
}

/// Racine temporaire du processus de test, créée une fois, sous le répertoire
/// temporaire du système — jamais dans l'arbre source.
///
/// Partagée par tous les tests du binaire, elle ne peut pas vivre dans un
/// garde à `Drop` (un `static` ne se détruit pas) : elle est supprimée à la
/// sortie du binaire par un `atexit` posé à sa création (Unix). #5512 : c'est
/// ce qui la nettoie aussi dans les binaires d'intégration, où `garde_de_sortie`
/// (compilé sous `cfg(test)`) n'existe pas.
fn racine() -> &'static Path {
    RACINE.get_or_init(|| {
        use tune_core::test_scratch::scratch_dir;
        // tmp-autorise: dossier partagé par tout le binaire, supprimé par l'atexit posé ci-dessous
        let racine = scratch_dir("tune-server-lib-5467").renoncer_au_nettoyage();
        #[cfg(unix)]
        {
            extern "C" fn supprimer_la_racine() {
                if let Some(racine) = RACINE.get() {
                    let _ = std::fs::remove_dir_all(racine);
                }
            }
            // SAFETY: fonction `extern "C"` sans argument qui ne panique pas ;
            // `atexit` ne fait que l'enregistrer.
            unsafe {
                libc::atexit(supprimer_la_racine);
            }
        }
        racine
    })
}

/// #5512 — le rapport de scan en build de test : `tune-scan-report.json`
/// sous la racine temporaire du processus, et plus dans le répertoire
/// courant, c'est-à-dire la caisse.
///
/// `None` hors test : l'appelant garde alors le chemin de production.
pub(crate) fn chemin_du_rapport_de_scan() -> Option<PathBuf> {
    actif().then(|| racine().join("tune-scan-report.json"))
}

/// Le cache d'illustrations que voit `artwork_cache_dir()` en build de test,
/// quand `TUNE_ARTWORK_DIR` n'est pas posé.
///
/// `None` hors test (#5512 : ce module est désormais compilé partout).
pub(crate) fn dossier_illustrations() -> Option<PathBuf> {
    actif().then(|| racine().join("artwork_cache"))
}

/// Ancre dans un répertoire temporaire propre à cet `AppState` les chemins
/// RELATIFS de la configuration (`db_path`, `artwork_dir`).
///
/// Un chemin absolu (un test qui a déjà son `tempdir`) est laissé tel quel ;
/// `":memory:"` aussi, parce que des routes le comparent littéralement
/// (`routes/system/backup.rs`, `routes/system/database.rs`).
///
/// Hors test ([`actif`] faux), la configuration est rendue telle quelle.
pub(crate) fn isoler_config(mut config: TuneConfig) -> TuneConfig {
    if !actif() {
        return config;
    }
    static SUIVANT: AtomicUsize = AtomicUsize::new(0);
    let dossier = racine().join(format!("etat-{}", SUIVANT.fetch_add(1, Ordering::Relaxed)));
    // Créé d'emblée : certains écrivains (le rapport de scan) ne créent pas
    // le dossier parent de leur fichier.
    std::fs::create_dir_all(&dossier).expect("dossier temporaire de l'AppState (#5467)");
    let ancrer = |chemin: &mut String| {
        if chemin.as_str() != ":memory:" && Path::new(chemin.as_str()).is_relative() {
            *chemin = dossier.join(chemin.as_str()).to_string_lossy().into_owned();
        }
    };
    ancrer(&mut config.db_path);
    ancrer(&mut config.artwork_dir);
    config
}

/// La garde demandée par #5467 : **après** la suite, aucun des témoins n'est
/// apparu dans l'arbre.
///
/// Un test ne peut pas s'exécuter « après les autres » ; la vérification se
/// fait donc à la sortie du binaire de test (`atexit`). Elle est armée au
/// CHARGEMENT du binaire, avant le premier test (section `.init_array` sous
/// Linux, `__mod_init_func` sous macOS — ce que fait la caisse `ctor`, sans
/// dépendance de plus) : armée plus tard, par exemple au premier `AppState`,
/// elle laisserait passer la régression même qu'elle garde, une écriture
/// faite avant l'armement comptant comme un reste.
///
/// Seuls comptent les témoins ABSENTS à l'armement : un reste d'une exécution
/// antérieure ne fabrique pas de faux rouge. Un témoin apparu fait sortir le
/// binaire en 101 — le code d'un test échoué — avec la liste des chemins.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod garde_de_sortie {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// Les deux dossiers de #5467, et le rapport de scan de #5512.
    const TEMOINS: [&str; 3] = ["artwork_cache", "queue_state", "tune-scan-report.json"];

    static ABSENTS: OnceLock<Vec<PathBuf>> = OnceLock::new();

    #[cfg(target_os = "linux")]
    #[used]
    #[unsafe(link_section = ".init_array")]
    static ARMEMENT: extern "C" fn() = armer;

    #[cfg(target_os = "macos")]
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func")]
    static ARMEMENT: extern "C" fn() = armer;

    extern "C" fn armer() {
        let mut bases = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))];
        if let Ok(courant) = std::env::current_dir()
            && !bases.contains(&courant)
        {
            bases.push(courant);
        }
        let absents: Vec<PathBuf> = bases
            .iter()
            .flat_map(|base| TEMOINS.iter().map(move |t| base.join(t)))
            .filter(|chemin| !chemin.exists())
            .collect();
        if ABSENTS.set(absents).is_ok() {
            // SAFETY: `verifier` est une fonction `extern "C"` sans argument
            // qui ne panique pas ; `atexit` ne fait que l'enregistrer.
            unsafe {
                libc::atexit(verifier);
            }
        }
    }

    extern "C" fn verifier() {
        if let Some(racine) = super::RACINE.get() {
            let _ = std::fs::remove_dir_all(racine);
        }
        let apparus: Vec<&PathBuf> = ABSENTS
            .get()
            .into_iter()
            .flatten()
            .filter(|chemin| chemin.exists())
            .collect();
        if apparus.is_empty() {
            return;
        }
        for chemin in &apparus {
            eprintln!(
                "#5467 — les tests de la lib ont écrit dans l'arbre : {}",
                chemin.display()
            );
        }
        // SAFETY: sortie immédiate du processus, sans rappeler les autres
        // gestionnaires `atexit` ; c'est ce qu'on veut, la suite est finie.
        unsafe { libc::_exit(101) }
    }
}

/// Le témoin de #5467 : la forme de test la plus courante,
/// `AppState::new(":memory:", 0, Default::default())`, ne résout plus ses
/// chemins depuis le répertoire courant, et `artwork_cache_dir()` non plus.
#[cfg(test)]
#[tokio::test]
async fn un_appstate_de_test_ne_pointe_plus_vers_le_repertoire_courant() {
    let etat = crate::state::AppState::new(":memory:", 0, TuneConfig::default()).unwrap();
    for chemin in [&etat.config.db_path, &etat.config.artwork_dir] {
        assert!(
            Path::new(chemin).starts_with(racine()),
            "#5467 — chemin de configuration non isolé, résolu depuis l'arbre : {chemin}"
        );
    }
    if std::env::var_os("TUNE_ARTWORK_DIR").is_none() {
        let illustrations = crate::routes::library::artwork_cache_dir();
        assert!(
            illustrations.starts_with(racine()),
            "#5467 — cache d'illustrations non isolé : {}",
            illustrations.display()
        );
    }
}
