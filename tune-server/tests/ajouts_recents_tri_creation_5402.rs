//! #5402 — « Ajouts récents » se trie par date de création, sur demande.
//!
//! Le tri historique (date d'ajout, modification d'abord, #4546) reste le
//! défaut : un client qui ne demande rien voit ce qu'il voyait. `?tri=creation`
//! filtre ET trie sur `file_first_seen.created_at`, et retombe sur la date
//! d'ajout quand le système n'a donné aucune date de création (NFS, SMB,
//! Docker). Le résumé compte ces pistes, pour que l'écran le dise.
//!
//! Par la ROUTE MONTÉE, comme `ajouts_recents_fenetre.rs` : un handler qui
//! ignorerait `tri` fait tomber le test.
//!
//! | album  | date d'ajout | création | rang « modification » | rang « création » (7 j) |
//! |--------|--------------|----------|-----------------------|-------------------------|
//! | Alpha  | J-1          | J-5      | 1                     | 3                       |
//! | Bravo  | J-3          | J-1      | 3                     | 1                       |
//! | Charlie| J-2          | —        | 2                     | 2 (repli sur J-2)       |
//! | Delta  | J-4          | J-30     | 4                     | hors fenêtre            |

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

fn il_y_a(jours: f64) -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        - jours * 24.0 * 3600.0
}

fn album(state: &AppState, titre: &str, ajout_jours: f64, creation_jours: Option<f64>) {
    let b = &state.backend;
    b.execute(
        &format!("INSERT INTO albums (title, track_count) VALUES ('{titre}', 1)"),
        &[],
    )
    .expect("album");
    let chemin = format!("/musique/{titre}/01.flac");
    b.execute(
        &format!(
            "INSERT INTO tracks (title, album_id, file_path, file_mtime, duration_ms) \
             VALUES ('Piste de {titre}', (SELECT id FROM albums WHERE title = '{titre}'), \
                     '{chemin}', {}, 60000)",
            il_y_a(900.0)
        ),
        &[],
    )
    .expect("piste");
    let creation = creation_jours.map_or("NULL".to_string(), |j| il_y_a(j).to_string());
    b.execute(
        &format!(
            "INSERT INTO file_first_seen (file_path, first_seen_at, created_at) \
             VALUES ('{chemin}', {}, {creation})",
            il_y_a(ajout_jours)
        ),
        &[],
    )
    .expect("première vue");
}

fn bibliotheque() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState");
    album(&state, "Alpha", 1.0, Some(5.0));
    album(&state, "Bravo", 3.0, Some(1.0));
    album(&state, "Charlie", 2.0, None);
    album(&state, "Delta", 4.0, Some(30.0));
    state
}

async fn appel(state: &AppState, chemin: &str) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .expect("le routeur doit répondre");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1024 * 1024)
        .await
        .expect("corps lisible");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn titres(corps: &Value) -> Vec<String> {
    corps
        .as_array()
        .unwrap_or_else(|| panic!("un TABLEAU d'albums est attendu : {corps}"))
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Le témoin : sans paramètre, et avec `tri=modification`, l'ordre reste
/// celui de la date d'ajout.
#[tokio::test(flavor = "multi_thread")]
async fn i5402_sans_parametre_le_tri_reste_la_date_d_ajout() {
    let state = bibliotheque();
    let attendu = vec!["Alpha", "Charlie", "Bravo", "Delta"];
    for route in [
        "/api/v1/home/recently-added",
        "/api/v1/home/recently-added?tri=modification",
    ] {
        let (statut, corps) = appel(&state, route).await;
        assert_eq!(statut, StatusCode::OK, "{route} : {corps}");
        assert_eq!(titres(&corps), attendu, "{route}");
    }
}

/// `tri=creation` trie ET filtre sur la date de création, avec le repli sur
/// la date d'ajout pour la piste qui n'en a pas.
#[tokio::test(flavor = "multi_thread")]
async fn i5402_le_tri_par_creation_suit_la_date_de_creation() {
    let state = bibliotheque();
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added?tri=creation").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        titres(&corps),
        vec!["Bravo", "Charlie", "Alpha"],
        "Bravo créé hier passe en tête, Charlie retombe sur sa date d'ajout, \
         Delta (créé à J-30) sort de la fenêtre de 7 jours"
    );
    let bravo = &corps.as_array().unwrap()[0];
    assert!(
        bravo["added_at"].as_f64().unwrap() > il_y_a(1.5),
        "`added_at` porte la date du tri choisi : {bravo}"
    );
}

/// Le résumé suit le tri, dit le tri servi, et compte les pistes sans date de
/// création.
#[tokio::test(flavor = "multi_thread")]
async fn i5402_le_resume_compte_les_pistes_sans_date_de_creation() {
    let state = bibliotheque();
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added/summary").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["tri"], "modification", "{corps}");
    assert_eq!(corps["album_count"], 4, "{corps}");

    let (statut, corps) = appel(&state, "/api/v1/home/recently-added/summary?tri=creation").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["tri"], "creation", "{corps}");
    assert_eq!(corps["album_count"], 3, "Delta est hors fenêtre : {corps}");
    assert_eq!(
        corps["tracks_without_creation_date"], 1,
        "Charlie n'a pas de date de création : {corps}"
    );
}

/// Une valeur inconnue est refusée, jamais servie comme un autre tri.
#[tokio::test(flavor = "multi_thread")]
async fn i5402_un_tri_inconnu_est_refuse() {
    let state = bibliotheque();
    for route in [
        "/api/v1/home/recently-added?tri=nom",
        "/api/v1/home/recently-added/summary?tri=nom",
    ] {
        let (statut, corps) = appel(&state, route).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{route} : {corps}");
    }
}
