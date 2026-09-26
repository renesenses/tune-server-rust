//! #5086 — les quatre sondes de Support › Diagnostic ne doivent pas attendre
//! l'ÉCRIVAIN SQLite.
//!
//! Sevy (0.9.163, macOS) voit « Serveur : injoignable », « Base de données :
//! injoignable », « Analyse : — » et aucune ligne disque, alors que le
//! serveur répond. Côté web, les quatre sondes sont indépendantes et bornées
//! chacune à 8 s ; elles n'ont donc qu'une façon de tomber ENSEMBLE : un point
//! de passage commun. Il est ici. Chacune des quatre routes passait par la
//! connexion d'ÉCRITURE, unique :
//!
//! - `/system/health` : `SettingsRepo::get("server_name")`, lu par
//!   `query_one_strong` ;
//! - `/system/scan/status` : `get("scan_status")` et `get("scan_result")` ;
//! - `/system/admin/health` : `get("scan_status")` ;
//! - `/system/database/status` : `migrations::current_version`, qui prend
//!   `connection().lock()`.
//!
//! Un écrivain qui tient cette connexion plus de 8 s (le verrou « surveillé »
//! de #4924 existe justement parce que ça arrive) faisait donc échouer les
//! quatre d'un coup, pendant que tout ce qui lit par le pool continuait de
//! servir. Aucune de ces lectures n'a besoin de la connexion d'écriture : ce
//! sont des valeurs COMMITÉES (un nom de serveur, un statut de scan que
//! `SCAN_GATE` corrige de toute façon, une version de schéma).
//!
//! Base de FICHIER, pas `:memory:` : en mémoire, le pool de lecture et
//! l'écrivain sont une seule connexion, et l'épreuve ne distinguerait rien.

use std::future::Future;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::state::AppState;

/// Durée pendant laquelle l'écrivain simulé tient la connexion.
const DETENTION: Duration = Duration::from_secs(3);

/// Au-delà, la sonde a attendu l'écrivain. Une lecture par le pool sur une
/// base vide répond en quelques millisecondes.
const SEUIL: Duration = Duration::from_millis(1_000);

async fn chronometre<F: Future>(f: F) -> (Duration, F::Output) {
    let debut = Instant::now();
    let sortie = f.await;
    (debut.elapsed(), sortie)
}

/// Prend la connexion d'écriture sur un fil à part et la garde
/// [`DETENTION`] ; rend la main une fois le verrou effectivement tenu.
fn tenir_l_ecrivain(state: &AppState) -> std::thread::JoinHandle<()> {
    let tenant = state.clone();
    let (pris_tx, pris_rx) = std::sync::mpsc::channel();
    let fil = std::thread::spawn(move || {
        let db = tenant.db.as_ref().expect("moteur SQLite");
        let _garde = db.connection().lock().unwrap();
        pris_tx.send(()).unwrap();
        std::thread::sleep(DETENTION);
    });
    pris_rx.recv().expect("l'écrivain a pris la connexion");
    fil
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_quatre_sondes_du_diagnostic_n_attendent_pas_l_ecrivain() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();

    let ecrivain = tenir_l_ecrivain(&state);

    let s = state.clone();
    let sante = tokio::spawn(async move {
        let (d, r) = chronometre(super::config::health(State(s))).await;
        (d, r.into_response().status())
    });
    let s = state.clone();
    let base = tokio::spawn(async move {
        let (d, r) = chronometre(super::database::database_status(State(s))).await;
        (
            d,
            r.map(|j| j.0["migration_version"].as_i64()).ok().flatten(),
        )
    });
    let s = state.clone();
    let analyse = tokio::spawn(async move {
        let (d, r) = chronometre(super::scan::scan_status(State(s))).await;
        (d, r.0["status"].as_str().map(str::to_owned))
    });
    let s = state.clone();
    let admin = tokio::spawn(async move {
        let (d, r) = chronometre(super::admin::admin_health(State(s))).await;
        (d, r.0["status"].as_str().map(str::to_owned))
    });

    let (d_sante, statut_sante) = sante.await.unwrap();
    let (d_base, version) = base.await.unwrap();
    let (d_analyse, statut_analyse) = analyse.await.unwrap();
    let (d_admin, statut_admin) = admin.await.unwrap();
    ecrivain.join().unwrap();

    let durees = format!(
        "health {d_sante:?}, database/status {d_base:?}, scan/status {d_analyse:?}, \
         admin/health {d_admin:?} (écrivain tenu {DETENTION:?})"
    );
    assert!(
        d_sante < SEUIL,
        "/system/health a attendu l'écrivain : {durees}"
    );
    assert!(
        d_base < SEUIL,
        "/system/database/status a attendu l'écrivain : {durees}"
    );
    assert!(
        d_analyse < SEUIL,
        "/system/scan/status a attendu l'écrivain : {durees}"
    );
    assert!(
        d_admin < SEUIL,
        "/system/admin/health a attendu l'écrivain : {durees}"
    );

    // Et elles disent quelque chose de juste, pas un repli muet.
    assert_eq!(statut_sante, StatusCode::OK);
    assert_eq!(
        version,
        Some(i64::from(tune_core::db::migrations::latest_version())),
        "la version de schéma lue par le pool est celle de la base migrée"
    );
    // `idle` ou `scanning` : `SCAN_GATE` est global au binaire de tests.
    assert!(statut_analyse.is_some());
    assert_eq!(statut_admin.as_deref(), Some("ok"));
}

/// Témoin : sans écrivain, les quatre répondent vite. Si ce témoin rougit,
/// c'est la machine qui est lente, pas le correctif qui manque.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temoin_sans_ecrivain_les_quatre_sondes_repondent_vite() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();

    let (d1, _) = chronometre(super::config::health(State(state.clone()))).await;
    let (d2, _) = chronometre(super::database::database_status(State(state.clone()))).await;
    let (d3, _) = chronometre(super::scan::scan_status(State(state.clone()))).await;
    let (d4, _) = chronometre(super::admin::admin_health(State(state.clone()))).await;
    for d in [d1, d2, d3, d4] {
        assert!(d < SEUIL, "{d:?}");
    }
}
