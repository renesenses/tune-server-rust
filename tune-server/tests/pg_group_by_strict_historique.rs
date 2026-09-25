//! `GROUP BY` lâche dans l'historique d'écoute, sur PostgreSQL (chasse PG du
//! 25/09/2026).
//!
//! SQLite accepte une colonne NUE hors du `GROUP BY` (il prend la valeur d'une
//! ligne quelconque du groupe). PostgreSQL la refuse et la requête ENTIÈRE
//! tombe. Deux requêtes de `history_repo.rs` étaient écrites ainsi — mesuré
//! sur PostgreSQL 16.15 :
//!
//! ```text
//! ERROR:  column "t.id" must appear in the GROUP BY clause or be used in an aggregate function
//! STATEMENT:  SELECT h.title, h.artist_name, COUNT(*) as plays, COALESCE(t.id, MAX(t3.id), h.track_id) …
//! ERROR:  column "listen_history.album_title" must appear in the GROUP BY clause …
//! STATEMENT:  SELECT title, artist_name, album_title, NULL, listened_at, … GROUP BY title, artist_name
//! ```
//!
//! Conséquence sur toute installation PostgreSQL : « Titres les plus écoutés »
//! (`/history/top-tracks`, l'accueil) et « Ce jour-là » du tableau de bord
//! d'écoute sont TOUJOURS vides — les routes avalent l'erreur et rendent
//! `200 []`. `pg_routes_serveur` sonde `/history/top-tracks` mais ne regarde
//! que le statut, sur une bibliothèque vide : il ne pouvait pas le voir.
//!
//! Le même scénario tourne sur SQLite (suite ordinaire) et sur PostgreSQL
//! (`TUNE_TEST_PG_URL`), et exige les mêmes lignes.

use axum::body::Body;
use axum::http::Request;
use chrono::Datelike;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn corps(state: &AppState, route: &str) -> Value {
    let app = tune_server::routes::router(state.clone());
    let rep = app
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(rep.status().is_success(), "{route} : {}", rep.status());
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

/// Sème trois écoutes d'un titre et une d'un autre, plus une écoute datée du
/// même jour il y a deux ans (« Ce jour-là »). Rend les écarts constatés.
async fn scenario(state: &AppState) -> Vec<String> {
    for table in ["listen_history"] {
        state
            .backend
            .execute(&format!("DELETE FROM {table}"), &[])
            .unwrap_or_else(|e| panic!("vidage de {table} : {e}"));
    }
    let maintenant = chrono::Utc::now();
    let il_y_a_deux_ans = format!(
        "{:04}-{:02}-{:02}T12:00:00Z",
        maintenant.year() - 2,
        maintenant.month(),
        maintenant.day().min(28)
    );
    let recent = maintenant.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let semence = [
        format!(
            "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at) \
             VALUES ('Moving', 'Kate Bush', 'The Kick Inside', 'local', '{recent}')"
        ),
        format!(
            "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at) \
             VALUES ('Moving', 'Kate Bush', 'The Kick Inside', 'local', '{recent}')"
        ),
        format!(
            "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at) \
             VALUES ('Moving', 'Kate Bush', 'The Kick Inside', 'local', '{recent}')"
        ),
        format!(
            "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at) \
             VALUES ('Sinnerman', 'Nina Simone', 'Pastel Blues', 'local', '{recent}')"
        ),
        format!(
            "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at) \
             VALUES ('Wuthering Heights', 'Kate Bush', 'The Kick Inside', 'local', '{il_y_a_deux_ans}')"
        ),
    ];
    for sql in &semence {
        state
            .backend
            .execute(sql, &[])
            .unwrap_or_else(|e| panic!("semence : {sql}\n{e}"));
    }

    let mut ecarts = Vec::new();

    let top = corps(state, "/api/v1/history/top-tracks?limit=10").await;
    let premier = top
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    if premier["title"] != "Moving" || premier["plays"].as_i64() != Some(3) {
        ecarts.push(format!("top-tracks : premier = {premier} (liste {top})"));
    }
    if premier["album_title"] != "The Kick Inside" {
        ecarts.push(format!(
            "top-tracks : album du premier = {}",
            premier["album_title"]
        ));
    }

    let tableau = corps(state, "/api/v1/history/dashboard?period=all").await;
    let ce_jour = tableau["on_this_day"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let titres: Vec<&str> = ce_jour
        .iter()
        .filter_map(|e| e["track_title"].as_str())
        .collect();
    if titres != vec!["Wuthering Heights"] {
        ecarts.push(format!(
            "dashboard on_this_day : {titres:?} ({})",
            tableau["on_this_day"]
        ));
    }
    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn group_by_de_l_historique_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_group_by_de_l_historique() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
