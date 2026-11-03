//! #6688 — le badge « Partagée avec un cercle » des playlists locales.
//!
//! Le greffon Tune Circle garde, pour chaque playlist de cercle qui suit une
//! playlist locale, un lien dans les réglages (`circle_suivi:{id}`, voir
//! `tune_circle::suivi`). `GET /playlists` et `GET /playlists/{id}` en
//! tirent `cercles: [{ id, nom, suivi, synchronisee_le }]` : vide pour une
//! playlist qui n'est pas partagée, `suivi` faux après « Ne plus suivre ».
//!
//! Le lien est écrit ici sous la forme exacte que le greffon persiste
//! (`Lien::en_json`, gardée côté greffon par
//! `le_lien_persiste_les_cles_que_lit_le_serveur`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn lis(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn playlist(state: &tune_server::state::AppState, nom: &str) -> i64 {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .create(nom, None, 1)
        .unwrap()
}

/// Un lien de suivi, à la forme du greffon.
fn lier(
    state: &tune_server::state::AppState,
    playlist_de_cercle: &str,
    locale: i64,
    cercle: (i64, &str),
    arrete: bool,
    synchro: Option<&str>,
) {
    let lien = json!({
        "circle_playlist_id": playlist_de_cercle,
        "local_playlist_id": locale,
        "circle_id": cercle.0,
        "circle_name": cercle.1,
        "stopped_at": if arrete { json!("2026-10-21T10:00:00Z") } else { Value::Null },
        "local_name": "peu importe",
        "followed_at": "2026-10-21T09:00:00Z",
        "last_sync": synchro,
        "pending": false,
    });
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("circle_suivi:{playlist_de_cercle}"),
            &lien.to_string(),
        )
        .unwrap();
}

#[tokio::test]
async fn la_liste_et_la_fiche_exposent_les_cercles_d_une_playlist_partagee_6688() {
    let state = etat();
    let partagee = playlist(&state, "Dimanche");
    let seule = playlist(&state, "Pour moi");
    lier(
        &state,
        "6",
        partagee,
        (1, "Famille"),
        false,
        Some("2026-10-21T09:30:00Z"),
    );
    lier(
        &state,
        "9",
        partagee,
        (2, "Jazz"),
        true,
        Some("2026-10-20T18:00:00Z"),
    );
    let app = tune_server::routes::router(state.clone());

    let (st, liste) = lis(&app, "/api/v1/playlists").await;
    assert_eq!(st, StatusCode::OK);
    let ligne = |id: i64| {
        liste
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap()
    };
    let attendu = json!([
        { "id": 1, "nom": "Famille", "suivi": true, "synchronisee_le": "2026-10-21T09:30:00Z" },
        { "id": 2, "nom": "Jazz", "suivi": false, "synchronisee_le": "2026-10-20T18:00:00Z" },
    ]);
    assert_eq!(
        ligne(partagee)["cercles"],
        attendu,
        "#6688 — la liste ne dit pas avec quels cercles la playlist est partagée"
    );
    assert_eq!(
        ligne(seule)["cercles"],
        json!([]),
        "#6688 — une playlist non partagée doit rendre `cercles: []`"
    );
    // Les champs d'avant restent.
    assert_eq!(ligne(partagee)["name"], "Dimanche");

    let (st, fiche) = lis(&app, &format!("/api/v1/playlists/{partagee}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        fiche["cercles"], attendu,
        "#6688 — la fiche ne dit pas avec quels cercles la playlist est partagée"
    );
    let (_, fiche) = lis(&app, &format!("/api/v1/playlists/{seule}")).await;
    assert_eq!(fiche["cercles"], json!([]));
}

/// Un réglage illisible, ou un lien vers une playlist d'un autre profil, ne
/// fait ni tomber la liste ni apparaître de badge ailleurs.
#[tokio::test]
async fn un_lien_illisible_ne_casse_rien_6688() {
    let state = etat();
    let p = playlist(&state, "Dimanche");
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("circle_suivi:7", "pas du json")
        .unwrap();
    lier(&state, "8", 99_999, (1, "Famille"), false, None);
    let app = tune_server::routes::router(state.clone());
    let (st, liste) = lis(&app, "/api/v1/playlists").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(liste[0]["id"], p);
    assert_eq!(liste[0]["cercles"], json!([]));
}
