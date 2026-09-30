//! #5512 — les tests d'INTÉGRATION n'écrivent plus dans l'arbre source.
//!
//! #5467 avait isolé les chemins disque des tests de la lib, mais sous
//! `#[cfg(test)]` : ce drapeau ne s'allume que pour la caisse testée
//! elle-même. Un binaire de `tune-server/tests/` lie la lib construite SANS
//! lui ; il gardait donc la `TuneConfig` par défaut, aux chemins RELATIFS
//! (`tune.db`, `artwork_cache`), résolus depuis le répertoire courant — la
//! caisse, où `cargo test` lance chaque binaire. D'où les `artwork_cache/`,
//! `queue_state/` et `tune-scan-report.json` qui réapparaissaient après les
//! tests.
//!
//! Ce fichier est un binaire d'intégration comme les autres : ce qu'il voit
//! est ce que voient les deux cents autres.

use std::path::Path;

use tune_server::config::TuneConfig;
use tune_server::state::AppState;

/// Un chemin est « dans l'arbre » s'il est relatif (résolu depuis le
/// répertoire courant) ou s'il tombe sous la caisse ou sous le répertoire
/// courant.
fn dans_l_arbre(chemin: &Path) -> bool {
    let caisse = Path::new(env!("CARGO_MANIFEST_DIR"));
    let courant = std::env::current_dir().expect("répertoire courant");
    chemin.is_relative() || chemin.starts_with(caisse) || chemin.starts_with(&courant)
}

/// La forme de plus de deux cents tests d'intégration : ses chemins de
/// configuration — `queue_state/` se dérive de `db_path` — ne pointent plus
/// vers le répertoire courant.
#[test]
fn un_appstate_d_integration_ne_pointe_plus_vers_l_arbre() {
    let etat = AppState::new(":memory:", 0, TuneConfig::default()).unwrap();
    for chemin in [&etat.config.db_path, &etat.config.artwork_dir] {
        assert!(
            !dans_l_arbre(Path::new(chemin)),
            "#5512 — chemin de configuration résolu dans l'arbre par un test d'intégration : {chemin}"
        );
    }
}

/// `artwork_cache_dir()` est une fonction libre, lue par les routes de
/// pochettes, le scan et les greffons : elle ne passe pas par `AppState`.
#[test]
fn le_cache_de_pochettes_d_integration_n_est_pas_dans_l_arbre() {
    if std::env::var_os("TUNE_ARTWORK_DIR").is_some() {
        return; // l'environnement a tranché, le test n'a rien à dire
    }
    let dossier = tune_server::routes::library::artwork_cache_dir();
    assert!(
        !dans_l_arbre(&dossier),
        "#5512 — cache de pochettes dans l'arbre : {}",
        dossier.display()
    );
}

/// Le rapport de scan ne dérive pas de la configuration mais de
/// `TUNE_DB_PATH` ou du littéral `"tune.db"` : il lui fallait son propre
/// détournement.
#[test]
fn le_rapport_de_scan_d_integration_n_est_pas_dans_l_arbre() {
    if std::env::var_os("TUNE_DB_PATH").is_some() {
        return;
    }
    let rapport = tune_server::routes::system::scan::chemin_du_rapport_de_scan();
    assert!(
        !dans_l_arbre(Path::new(&rapport)),
        "#5512 — rapport de scan dans l'arbre : {rapport}"
    );
}
