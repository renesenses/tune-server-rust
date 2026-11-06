//! #7769 — trois écrans, un seul nombre d'écoutes.
//!
//! Les écoutes sont écrites comme le fait la production
//! (`orchestrator::commun::record_listen`) : titre, interprète, album,
//! `track_id` VIDE. Pistes « Trois » (3 écoutes), « Une » (1) et « Zéro » (0),
//! plus une écoute radio de « Zéro » qui ne compte pas.
//!
//! - les règles « nombre d'écoutes » d'une liste intelligente rendent
//!   exactement les bonnes pistes (elles lisaient `track_id` : « = 0 » rendait
//!   tout, « < 1 » rien) ;
//! - l'Historique (`GET /library/history`) porte, pour chaque écoute, le même
//!   `play_count` que la Bibliothèque (`GET /library/tracks`) — la colonne
//!   « # Plays » de l'Historique n'avait aucun nombre à afficher.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

async fn appel(state: &AppState, methode: &str, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let resp = tune_server::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Pistes 1 « Trois », 2 « Une », 3 « Zéro », du même interprète et du même
/// album ; leurs écoutes sans `track_id`.
fn bibliotheque() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Bill Evans')",
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Sunday', 1)",
        "INSERT INTO tracks (id, album_id, artist_id, title, file_path) VALUES \
           (1, 1, 1, 'Trois', '/m/1.flac'), (2, 1, 1, 'Une', '/m/2.flac'), \
           (3, 1, 1, 'Zéro', '/m/3.flac')",
        "INSERT INTO listen_history (title, artist_name, album_title, album_id, source, listened_at) VALUES \
           ('Trois', 'Bill Evans', 'Sunday', 1, 'local', '2026-11-01T10:00:00Z'), \
           ('Trois', 'Bill Evans', 'Sunday', 1, 'local', '2026-11-01T11:00:00Z'), \
           ('Trois', 'Bill Evans', 'Sunday', 1, 'local', '2026-11-01T12:00:00Z'), \
           ('Une', 'Bill Evans', 'Sunday', 1, 'local', '2026-11-01T13:00:00Z'), \
           ('Zéro', 'Bill Evans', NULL, NULL, 'radio', '2026-11-01T14:00:00Z')",
    ] {
        state.backend.execute(sql, &[]).unwrap();
    }
    state
}

async fn pistes_de_la_regle(state: &AppState, op: &str, valeur: Value) -> Vec<i64> {
    let (st, corps) = appel(
        state,
        "POST",
        "/api/v1/library/smart-playlists/preview",
        json!({"rules": [{"field": "play_count", "op": op, "value": valeur}]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{corps}");
    assert_eq!(corps["regles_refusees"], json!([]), "{corps}");
    let mut ids: Vec<i64> = corps["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    ids.sort();
    ids
}

/// 🔴 LE TÉMOIN des listes intelligentes : « = 0 », « < 1 », « > 2 » et
/// « = 3 » rendent chacune exactement les bonnes pistes.
#[tokio::test]
async fn les_regles_d_ecoutes_rendent_les_bonnes_pistes_7769() {
    let state = bibliotheque();
    for (op, valeur, attendu) in [
        ("=", json!(0), vec![3]),
        ("<", json!(1), vec![3]),
        (">", json!(2), vec![1]),
        ("=", json!(3), vec![1]),
    ] {
        assert_eq!(
            pistes_de_la_regle(&state, op, valeur.clone()).await,
            attendu,
            "play_count {op} {valeur} : mauvaises pistes"
        );
    }
}

/// 🔴 LE TÉMOIN de l'Historique : chaque écoute porte `play_count`, égal à
/// celui que la Bibliothèque affiche pour la même piste.
#[tokio::test]
async fn l_historique_donne_le_nombre_d_ecoutes_de_la_bibliotheque_7769() {
    let state = bibliotheque();

    let (st, bib) = appel(
        &state,
        "GET",
        "/api/v1/library/tracks?limit=100",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{bib}");
    let dans_la_bibliotheque: HashMap<i64, i64> = bib["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_i64().unwrap(), p["play_count"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        dans_la_bibliotheque,
        HashMap::from([(1, 3), (2, 1), (3, 0)]),
        "le banc ne donne plus 3, 1 et 0 écoutes dans la Bibliothèque"
    );

    let (st, hist) = appel(
        &state,
        "GET",
        "/api/v1/library/history?limit=50",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{hist}");
    let ecoutes = hist["items"].as_array().unwrap();
    assert_eq!(ecoutes.len(), 4, "la radio n'est pas listée : {hist}");
    for e in ecoutes {
        let piste = e["track_id"].as_i64().expect("écoute locale résolue");
        assert_eq!(
            e["play_count"].as_i64(),
            Some(dans_la_bibliotheque[&piste]),
            "l'Historique ne donne pas le nombre d'écoutes de la Bibliothèque pour {e}"
        );
    }
}
