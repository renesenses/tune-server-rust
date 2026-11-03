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

/// TÉMOIN — sans type secondaire, seul le TITRE range dans « Live » (#6464) :
/// « Live Rust » et « Live EP » le disent, « Weld » non et reste un album.
/// Avant #6464, la section était absente dans ce cas.
#[tokio::test]
async fn sans_type_secondaire_seul_le_titre_range_dans_live_6464() {
    let app = bibliotheque(false);
    let (_, body) = get(&app, "/api/v1/library/artists/1/albums?sections=1").await;
    assert_eq!(
        titres(&body["live"]),
        vec!["Live EP", "Live Rust"],
        "{body}"
    );
    assert_eq!(
        titres(&body["albums"]),
        vec!["Harvest", "Remixes", "Weld"],
        "{body}"
    );
    for a in body["live"].as_array().unwrap() {
        assert_eq!(a["live_by_title"], true, "{a}");
    }
}

/// #6464 — la règle provisoire par le titre (FabienM, fil 2302 point 4) :
///
/// | id | titre                     | primaire | secondaires       | section  |
/// |----|---------------------------|----------|-------------------|----------|
/// | 1  | MTV Unplugged             | ep       | —                 | live     |
/// | 2  | Live Through This         | album    | —                 | albums   |
/// | 3  | Live Remixes              | album    | compilation;remix | albums   |
/// | 4  | Live at Leeds             | album    | live              | live     |
///
/// Un EP live va dans « Live » ; « live » verbe n'y va pas ; un type
/// secondaire CONNU sans `live` n'est pas contredit par le titre ; le drapeau
/// `live_by_title` dit quand la règle provisoire a décidé.
#[tokio::test]
async fn la_regle_du_titre_complete_les_types_sans_les_contredire_6464() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    exec(
        &state,
        "INSERT INTO artists (id, name) VALUES (1, 'Divers')",
    );
    let albums: [(i64, &str, &str, Option<&str>); 4] = [
        (1, "MTV Unplugged", "ep", None),
        (2, "Live Through This", "album", None),
        (3, "Live Remixes", "album", Some("compilation;remix")),
        (4, "Live at Leeds", "album", Some("live")),
    ];
    for (id, titre, primaire, secondaires) in albums {
        exec(
            &state,
            &format!(
                "INSERT INTO albums (id, title, artist_id, release_type, release_secondary_types, year, source) \
                 VALUES ({id}, '{titre}', 1, '{primaire}', {}, 2000, 'local')",
                secondaires.map_or("NULL".to_string(), |t| format!("'{t}'"))
            ),
        );
        exec(
            &state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format) \
                 VALUES ({id}, 't{id}', {id}, 1, '/m/{id}.flac', 240000, 'flac')"
            ),
        );
    }
    let app = tune_server::routes::router(state);
    let (status, body) = get(&app, "/api/v1/library/artists/1/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        titres(&body["live"]),
        vec!["Live at Leeds", "MTV Unplugged"],
        "seuls le live typé et l'EP « Unplugged » vont dans « Live » ; un type \
         secondaire connu sans `live` n'est pas contredit par le titre — {body}"
    );
    assert_eq!(
        titres(&body["albums"]),
        vec!["Live Remixes", "Live Through This"],
        "un type secondaire CONNU sans `live` (Live Remixes) n'est pas contredit \
         par le titre, et « live » verbe (Live Through This) ne range rien — {body}"
    );
    let live = body["live"].as_array().unwrap();
    let drapeau = |t: &str| live.iter().find(|a| a["title"] == t).unwrap()["live_by_title"].clone();
    assert_eq!(drapeau("MTV Unplugged"), true);
    assert_eq!(
        drapeau("Live at Leeds"),
        Value::Null,
        "le type secondaire a décidé, pas le titre"
    );
}
