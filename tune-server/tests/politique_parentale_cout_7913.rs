//! #7913 — le coût du total exact d'une liste paginée pour un profil
//! supervisé. Un seul témoin dans ce binaire : le compteur de pages relues
//! est global.
//!
//! Banc : 3 000 albums libres, 12 d'un artiste bloqué. La première page
//! relit la liste (pages de 500) ; feuilleter ensuite ne la relit plus (cache
//! par profil, liste et révision de la politique) ; un album ajouté la fait
//! relire une fois.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_server::routes::politique_parentale::PAGES_RELUES;

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const LIBRES: i64 = 3000;
const BLOQUES: i64 = 12;

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: i64,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("X-Profile-Id", profil.to_string())
        .header("content-type", "application/json")
        .body(
            corps
                .map(|c| Body::from(c.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let reponse = app.clone().oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn le_total_exact_ne_relit_la_liste_qu_une_fois_7913() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let db = &state.backend;
    db.execute(
        "INSERT INTO profiles (id, username, display_name, is_admin) VALUES (2, 'enfant', 'Enfant', 0)",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO artists (id, name) VALUES (7, 'Groupe Interdit'), (8, 'Groupe Libre')",
        &[],
    )
    .unwrap();
    db.execute_batch("BEGIN").ok();
    for i in 0..(LIBRES + BLOQUES) {
        let artiste: i64 = if i < BLOQUES { 7 } else { 8 };
        let titre = format!("Album {i:05}");
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[&(i + 1) as &dyn ToSqlValue, &titre, &artiste],
        )
        .unwrap();
        let chemin = format!("/musique/{i}.flac");
        db.execute(
            "INSERT INTO tracks (title, artist_id, album_id, file_path, format) VALUES (?, ?, ?, ?, 'flac')",
            &[&titre as &dyn ToSqlValue, &artiste, &(i + 1), &chemin],
        )
        .unwrap();
    }
    db.execute_batch("COMMIT").ok();
    let app = tune_server::routes::router(state.clone());
    let (statut, corps) = appel(
        &app,
        "PUT",
        "/api/v1/profiles/2/politique-parentale",
        ADMIN,
        Some(json!({"revision": 0, "politique": {"artistes_bloques": [{"id": 7}]}})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    let t = Instant::now();
    let (_, admin) = appel(
        &app,
        "GET",
        "/api/v1/library/albums?limit=50&offset=50",
        ADMIN,
        None,
    )
    .await;
    let ms_admin = t.elapsed().as_millis();
    assert_eq!(admin["total"], LIBRES + BLOQUES, "{}", admin["total"]);

    let avant = PAGES_RELUES.load(Ordering::Relaxed);
    let t = Instant::now();
    let (_, p2) = appel(
        &app,
        "GET",
        "/api/v1/library/albums?limit=50&offset=50",
        ENFANT,
        None,
    )
    .await;
    let ms_premiere = t.elapsed().as_millis();
    let relues_premiere = PAGES_RELUES.load(Ordering::Relaxed) - avant;
    assert_eq!(p2["total"], LIBRES, "#7913 : total exact : {}", p2["total"]);
    assert_eq!(relues_premiere, 7, "3 012 albums relus par pages de 500");

    // Feuilleter : aucune relecture.
    let avant = PAGES_RELUES.load(Ordering::Relaxed);
    let t = Instant::now();
    for offset in [100, 150, 200, 250] {
        let (_, p) = appel(
            &app,
            "GET",
            &format!("/api/v1/library/albums?limit=50&offset={offset}"),
            ENFANT,
            None,
        )
        .await;
        assert_eq!(p["total"], LIBRES);
    }
    let ms_suivantes = t.elapsed().as_millis() / 4;
    assert_eq!(
        PAGES_RELUES.load(Ordering::Relaxed) - avant,
        0,
        "#7913 : feuilleter ne relit plus la liste"
    );

    // Un album bloqué de plus : la bibliothèque a changé, une seule relecture.
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (99999, 'Album nouveau', 7)",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO tracks (title, artist_id, album_id, file_path, format) VALUES ('Nouveau', 7, 99999, '/musique/n.flac', 'flac')",
        &[],
    )
    .unwrap();
    let avant = PAGES_RELUES.load(Ordering::Relaxed);
    let (_, p) = appel(
        &app,
        "GET",
        "/api/v1/library/albums?limit=50&offset=50",
        ENFANT,
        None,
    )
    .await;
    assert_eq!(p["total"], LIBRES, "{}", p["total"]);
    assert_eq!(PAGES_RELUES.load(Ordering::Relaxed) - avant, 7);

    eprintln!(
        "MESURE #7913 ({} albums) : administrateur {ms_admin} ms ; profil supervisé, première page {ms_premiere} ms ({relues_premiere} pages relues) ; pages suivantes {ms_suivantes} ms (0 relue)",
        LIBRES + BLOQUES
    );
}
