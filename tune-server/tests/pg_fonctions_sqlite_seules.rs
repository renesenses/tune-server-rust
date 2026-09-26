//! Du SQL propre à SQLite dans des routes jouées sur PostgreSQL (chasse PG du
//! 25/09/2026).
//!
//! Deux sites, deux écritures que SQLite avale et que PostgreSQL refuse, toutes
//! deux AVALÉES par leur appelant — aucune erreur visible, seulement une
//! fonction morte :
//!
//! 1. `api_cache_get` (`routes/library/mod.rs`) mesurait la fraîcheur du cache
//!    avec `strftime('%s', …)` : « function strftime(unknown, unknown) does not
//!    exist ». Le `.ok()?` rendait `None` : le cache des biographies d'artiste
//!    et d'album, des artistes similaires et des métadonnées n'était JAMAIS
//!    relu, chaque visite rappelait le service distant.
//! 2. `POST /library/albums/merge-duplicates` agrégeait `STRING_AGG($1, ',')`
//!    — un paramètre, pas la colonne `id` — dans une requête qui ne lie rien.
//!    La recherche des doublons échouait, la route rendait `merged: 0` et le
//!    bouton « Fusionner les doublons » ne faisait rien.
//!
//! Le même scénario tourne sur SQLite (suite ordinaire) et sur PostgreSQL
//! (`TUNE_TEST_PG_URL`).

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn appel(state: &AppState, methode: &str, route: &str) -> (u16, String) {
    let app = tune_server::routes::router(state.clone());
    let rep = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(route)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status().as_u16();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (statut, String::from_utf8_lossy(&octets).to_string())
}

fn scalaire(state: &AppState, sql: &str) -> i64 {
    state
        .backend
        .query_one(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"))
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

async fn scenario(state: &AppState) -> Vec<String> {
    let mut ecarts = Vec::new();
    let maintenant = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // ── 1. Le cache d'API est relu ──
    state
        .backend
        .execute(
            "DELETE FROM settings WHERE key LIKE 'cache:artistbio:Sonde Cache%'",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute("INSERT INTO artists (name) VALUES ('Sonde Cache')", &[])
        .unwrap();
    let artiste = scalaire(
        state,
        "SELECT MAX(id) FROM artists WHERE name = 'Sonde Cache'",
    );
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO settings (key, value, updated_at) VALUES \
                 ('cache:artistbio:Sonde Cache:fr', \
                  '{{\"artist\":\"Sonde Cache\",\"bio\":\"BIO-DU-CACHE\"}}', '{maintenant}')"
            ),
            &[],
        )
        .unwrap();
    let (st, corps) = appel(
        state,
        "GET",
        &format!("/api/v1/library/artists/{artiste}/bio?lang=fr"),
    )
    .await;
    if st != 200 || !corps.contains("BIO-DU-CACHE") {
        ecarts.push(format!("bio servie hors du cache frais : {st} {corps}"));
    }

    // ── 2. La fusion manuelle des doublons trouve ses doublons ──
    state
        .backend
        .execute("INSERT INTO artists (name) VALUES ('Sonde Doublon')", &[])
        .unwrap();
    for titre in ["Kind of Blue Sonde", "kind of blue sonde"] {
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO albums (title, artist_id, source) \
                     SELECT '{titre}', MAX(id), 'local' FROM artists WHERE name = 'Sonde Doublon'"
                ),
                &[],
            )
            .unwrap();
        state
            .backend
            .execute(
                &format!(
                    "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path, source) \
                     SELECT 'So What', al.id, al.artist_id, '545000', '/sonde/{titre}.flac', 'local' \
                     FROM albums al WHERE al.title = '{titre}'"
                ),
                &[],
            )
            .unwrap();
    }
    let (st, corps) = appel(state, "POST", "/api/v1/library/albums/merge-duplicates").await;
    let fusionnes = serde_json::from_str::<Value>(&corps)
        .ok()
        .and_then(|v| v["merged"].as_i64());
    if st != 200 || fusionnes.unwrap_or(0) < 1 {
        ecarts.push(format!("fusion des doublons : {st} {corps}"));
    }
    let restants = scalaire(
        state,
        "SELECT COUNT(*) FROM albums WHERE LOWER(title) = 'kind of blue sonde'",
    );
    if restants != 1 {
        ecarts.push(format!(
            "albums « kind of blue sonde » après fusion : {restants}"
        ));
    }
    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn fonctions_sqlite_seules_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_fonctions_sqlite_seules() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    // Une base partagée par les étapes : on repart d'albums sans « sonde ».
    for sql in [
        "DELETE FROM tracks WHERE file_path LIKE '/sonde/%'",
        "DELETE FROM albums WHERE LOWER(title) = 'kind of blue sonde'",
    ] {
        state.backend.execute(sql, &[]).unwrap();
    }
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
