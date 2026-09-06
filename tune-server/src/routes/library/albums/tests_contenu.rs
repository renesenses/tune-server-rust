//! BIB-B2 (phase C) : `same_content` dans `GET /library/albums/{id}/editions`.

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

use crate::routes::library::duplicates::tests_contenu::{empreinte_texte, signal};

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

fn album(state: &Etat, titre: &str) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO albums (title, artist_id, source, track_count) VALUES (?, 1, 'local', 2)",
            &[&titre as &dyn ToSqlValue],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

fn piste(state: &Etat, album_id: i64, numero: i64, chemin: &str, empreinte: Option<&str>) {
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path, duration_ms) \
             VALUES (?, ?, 1, ?, ?, 30000)",
            &[
                &format!("Piste {numero}") as &dyn ToSqlValue,
                &album_id,
                &numero,
                &chemin,
            ],
        )
        .unwrap();
    if let Some(e) = empreinte {
        state
            .backend
            .execute(
                "UPDATE tracks SET audio_fingerprint = ? WHERE file_path = ?",
                &[&e as &dyn ToSqlValue, &chemin],
            )
            .unwrap();
    }
}

async fn editions(app: &axum::Router, id: i64) -> Value {
    let reponse = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/library/albums/{id}/editions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

#[tokio::test]
async fn une_edition_de_meme_contenu_est_dite_telle_et_une_autre_non() {
    let (app, state) = serveur();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd')",
            &[],
        )
        .unwrap();
    let p1 = signal(&[220.0, 330.0, 440.0], 30.0, 0.8);
    let p2 = signal(&[110.0, 165.0], 30.0, 0.8);
    let autre = signal(&[1_000.0, 1_500.0], 30.0, 0.8);
    let p1_attenue: Vec<i32> = p1.iter().map(|s| s / 2).collect();

    let original = album(&state, "The Wall");
    piste(
        &state,
        original,
        1,
        "/m/wall/01.flac",
        Some(&empreinte_texte(&p1)),
    );
    piste(
        &state,
        original,
        2,
        "/m/wall/02.flac",
        Some(&empreinte_texte(&p2)),
    );
    // Même master, autre résolution : même contenu.
    let remaster = album(&state, "The Wall (Remastered 2011)");
    piste(
        &state,
        remaster,
        1,
        "/m/wall-hd/01.flac",
        Some(&empreinte_texte(&p1_attenue)),
    );
    piste(
        &state,
        remaster,
        2,
        "/m/wall-hd/02.flac",
        Some(&empreinte_texte(&p2)),
    );
    // Même titre d'édition, autre enregistrement (un live) : pas le même.
    let live = album(&state, "The Wall (Live)");
    piste(
        &state,
        live,
        1,
        "/m/wall-live/01.flac",
        Some(&empreinte_texte(&autre)),
    );
    piste(
        &state,
        live,
        2,
        "/m/wall-live/02.flac",
        Some(&empreinte_texte(&autre)),
    );
    // Sans empreinte : on ne sait pas.
    let inconnu = album(&state, "The Wall (Deluxe)");
    piste(&state, inconnu, 1, "/m/wall-dx/01.flac", None);
    piste(&state, inconnu, 2, "/m/wall-dx/02.flac", None);

    let corps = editions(&app, original).await;
    let par_id = |id: i64| -> Value {
        corps["editions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"].as_i64() == Some(id))
            .cloned()
            .unwrap_or_else(|| panic!("édition {id} absente : {corps}"))
    };
    assert_eq!(
        par_id(remaster)["same_content"],
        Value::Bool(true),
        "{corps}"
    );
    assert_eq!(par_id(remaster)["content_compared"], 2);
    assert_eq!(par_id(remaster)["content_same"], 2);
    assert_eq!(par_id(live)["same_content"], Value::Bool(false));
    assert_eq!(par_id(live)["content_same"], 0);
    assert_eq!(
        par_id(inconnu)["same_content"],
        Value::Null,
        "sans empreinte, on ne tranche pas"
    );
    assert_eq!(par_id(inconnu)["content_compared"], 0);
}
