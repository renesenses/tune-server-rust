//! Section « Live » de la fiche artiste (Bertrand, 05/10/2026, suite de
//! #5616) — `GET /library/artists/{id}/albums` publie les types SECONDAIRES
//! MusicBrainz (`albums.release_secondary_types`) sous
//! `release_secondary_types`, et, avec `sections=1`, range sous `live` tout
//! disque dont les types secondaires portent `live`, QUEL QUE SOIT son type
//! primaire. Ce disque n'est plus sous `albums`.
//!
//! Épreuves contre le VRAI routeur et une base SQLite en mémoire.
//! Cible `[[test]]` propre (`autotests = false`), hors de `server_contracts`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// | id | titre          | primaire | secondaires        |
/// |----|----------------|----------|--------------------|
/// | 1  | Harvest        | album    | —                  |
/// | 2  | Live Rust      | album    | live               |
/// | 3  | Live EP        | ep       | live               |
/// | 4  | Remixes        | album    | compilation;remix  |
/// | 5  | Weld           | —        | live               |
fn bibliotheque(avec_live: bool) -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    exec(
        &state,
        "INSERT INTO artists (id, name) VALUES (1, 'Neil Young')",
    );
    let albums: [(i64, &str, Option<&str>, Option<&str>); 5] = [
        (1, "Harvest", Some("album"), None),
        (2, "Live Rust", Some("album"), Some("live")),
        (3, "Live EP", Some("ep"), Some("live")),
        (4, "Remixes", Some("album"), Some("compilation;remix")),
        (5, "Weld", None, Some("live")),
    ];
    let sql = |v: Option<&str>| v.map_or("NULL".to_string(), |t| format!("'{t}'"));
    for (id, titre, primaire, secondaires) in albums {
        let secondaires = if avec_live { secondaires } else { None };
        exec(
            &state,
            &format!(
                "INSERT INTO albums (id, title, artist_id, release_type, release_secondary_types, year, source) \
                 VALUES ({id}, '{titre}', 1, {}, {}, 2000, 'local')",
                sql(primaire),
                sql(secondaires)
            ),
        );
        for n in 0..10 {
            let piste = id * 100 + n;
            exec(
                &state,
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format) \
                     VALUES ({piste}, 't{piste}', {id}, 1, '/m/{piste}.flac', 240000, 'flac')"
                ),
            );
        }
    }
    tune_server::routes::router(state)
}

fn titres(liste: &Value) -> Vec<String> {
    let mut t: Vec<String> = liste
        .as_array()
        .unwrap_or_else(|| panic!("tableau attendu : {liste}"))
        .iter()
        .map(|a| a["title"].as_str().unwrap().to_string())
        .collect();
    t.sort();
    t
}

#[tokio::test]
async fn un_live_va_dans_la_section_live_quel_que_soit_son_type_primaire() {
    let app = bibliotheque(true);
    let (status, body) = get(&app, "/api/v1/library/artists/1/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        titres(&body["live"]),
        vec!["Live EP", "Live Rust", "Weld"],
        "{body}"
    );
    assert_eq!(
        titres(&body["albums"]),
        vec!["Harvest", "Remixes"],
        "{body}"
    );

    let album = |liste: &Value, titre: &str| {
        liste
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["title"] == titre)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        album(&body["live"], "Live EP")["release_secondary_types"],
        serde_json::json!(["live"])
    );
    assert_eq!(album(&body["live"], "Live EP")["release_type"], "ep");
    assert_eq!(
        album(&body["albums"], "Remixes")["release_secondary_types"],
        serde_json::json!(["compilation", "remix"])
    );
    assert!(
        album(&body["albums"], "Harvest")
            .get("release_secondary_types")
            .is_none(),
        "clé absente quand rien n'est connu"
    );
}

#[tokio::test]
async fn le_tableau_nu_garde_les_lives_avec_leur_type_secondaire() {
    let app = bibliotheque(true);
    let (status, body) = get(&app, "/api/v1/library/artists/1/albums").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        titres(&body),
        vec!["Harvest", "Live EP", "Live Rust", "Remixes", "Weld"]
    );
    let weld = body
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Weld")
        .unwrap();
    assert_eq!(weld["release_secondary_types"], serde_json::json!(["live"]));
}

/// TÉMOIN — sans type secondaire, pas de section « Live » : la clé est absente.
#[tokio::test]
async fn sans_type_secondaire_la_section_live_est_absente() {
    let app = bibliotheque(false);
    let (_, body) = get(&app, "/api/v1/library/artists/1/albums?sections=1").await;
    assert!(body.get("live").is_none(), "{body}");
    assert_eq!(titres(&body["albums"]).len(), 5);
}
