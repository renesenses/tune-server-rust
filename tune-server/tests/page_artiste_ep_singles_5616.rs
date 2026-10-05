//! #5616 — la fiche artiste sépare Albums, EP et Singles même sans type
//! explicite : `GET /library/artists/{id}/albums` publie, à côté de
//! `release_type`, un `inferred_release_type` tiré de la règle pistes et
//! durée (`tune_core::metadata::release_type::type_deduit`).
//!
//! Épreuves contre le VRAI routeur et une base SQLite en mémoire :
//! - un disque sans type reçoit `single`, `ep` ou `album` selon ses pistes ;
//! - un type explicite gagne toujours : aucune clé déduite à côté ;
//! - une compilation n'est jamais déduite ;
//! - une durée de piste inconnue suspend la déduction ;
//! - le tableau nu (sans `sections=1`) porte le même champ.
//!
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

const MIN: i64 = 60_000;

/// | id | titre            | type explicite | compil. | pistes × durée     |
/// |----|------------------|----------------|---------|--------------------|
/// | 1  | Deux titres      | —              | non     | 2 × 4 min          |
/// | 2  | Cinq titres      | —              | non     | 5 × 4 min          |
/// | 3  | Dix titres       | —              | non     | 10 × 4 min         |
/// | 4  | Album MB court   | album          | non     | 2 × 4 min          |
/// | 5  | Single MB long   | single         | non     | 8 × 5 min          |
/// | 6  | Compil courte    | —              | OUI     | 2 × 4 min          |
/// | 7  | Durée inconnue   | —              | non     | 4 min + inconnue   |
/// | 8  | Trois longs      | —              | non     | 3 × 5 min (15 pile)|
fn bibliotheque() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    exec(
        &state,
        "INSERT INTO artists (id, name) VALUES (1, 'Fabien M')",
    );
    let albums: [(i64, &str, Option<&str>, i64, &[i64]); 8] = [
        (1, "Deux titres", None, 0, &[4 * MIN, 4 * MIN]),
        (2, "Cinq titres", None, 0, &[4 * MIN; 5]),
        (3, "Dix titres", None, 0, &[4 * MIN; 10]),
        (4, "Album MB court", Some("album"), 0, &[4 * MIN, 4 * MIN]),
        (5, "Single MB long", Some("single"), 0, &[5 * MIN; 8]),
        (6, "Compil courte", None, 1, &[4 * MIN, 4 * MIN]),
        (7, "Duree inconnue", None, 0, &[4 * MIN, 0]),
        (8, "Trois longs", None, 0, &[5 * MIN; 3]),
    ];
    let mut piste = 0;
    for (id, titre, rt, compil, durees) in albums {
        let rt = rt.map_or("NULL".to_string(), |t| format!("'{t}'"));
        exec(
            &state,
            &format!(
                "INSERT INTO albums (id, title, artist_id, is_compilation, release_type, year, source) \
                 VALUES ({id}, '{titre}', 1, {compil}, {rt}, 2000, 'local')"
            ),
        );
        for d in durees {
            piste += 1;
            exec(
                &state,
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format) \
                     VALUES ({piste}, 't{piste}', {id}, 1, '/m/{piste}.flac', {d}, 'flac')"
                ),
            );
        }
    }
    tune_server::routes::router(state)
}

/// Tous les albums de la réponse, quelle que soit la section.
fn tous(body: &Value) -> Vec<Value> {
    match body {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .cloned()
            .collect(),
        _ => panic!("réponse inattendue : {body}"),
    }
}

fn deduit(body: &Value, titre: &str) -> Option<String> {
    let albums = tous(body);
    let a = albums
        .iter()
        .find(|a| a["title"] == titre)
        .unwrap_or_else(|| panic!("`{titre}` absent de la réponse : {body}"));
    a.get("inferred_release_type")
        .map(|v| v.as_str().expect("chaîne").to_string())
}

async fn verifier(path: &str) {
    let app = bibliotheque();
    let (status, body) = get(&app, path).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let attendu: [(&str, Option<&str>); 8] = [
        ("Deux titres", Some("single")),
        ("Cinq titres", Some("ep")),
        ("Dix titres", Some("album")),
        ("Album MB court", None),
        ("Single MB long", None),
        ("Compil courte", None),
        ("Duree inconnue", None),
        ("Trois longs", Some("album")),
    ];
    for (titre, t) in attendu {
        assert_eq!(
            deduit(&body, titre).as_deref(),
            t,
            "`{titre}` ({path}) : {body}"
        );
    }
    // Le type explicite reste publié tel quel.
    let single_mb = tous(&body)
        .into_iter()
        .find(|a| a["title"] == "Single MB long")
        .unwrap();
    assert_eq!(single_mb["release_type"], "single");
}

#[tokio::test]
async fn la_fiche_artiste_publie_le_type_deduit_5616() {
    verifier("/api/v1/library/artists/1/albums?sections=1").await;
}

#[tokio::test]
async fn le_tableau_nu_porte_le_meme_champ_5616() {
    verifier("/api/v1/library/artists/1/albums").await;
}
