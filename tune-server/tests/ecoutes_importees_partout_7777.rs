//! Témoins de #7777 — les écoutes IMPORTÉES (Roon, Plex, m3u ; table
//! `imported_plays`, #7763) comptent ailleurs que dans la Bibliothèque.
//!
//! Avant le correctif, seule la colonne « Écoutes » de la Bibliothèque les
//! additionnait : « Titres les plus écoutés » (accueil, `/history/top-tracks`,
//! tableau de bord, widget) et les statistiques d'écoute (`/stats/listening` :
//! totaux, artistes, albums, titres) ne comptaient que `listen_history`. Une
//! piste écoutée 40 fois dans Roon et jamais dans Tune n'y figurait pas.
//!
//! Règle : les écoutes importées comptent dans tout ce qui est « tous temps »
//! et restent hors des vues par période, faute de date propre.
//!
//! SQLite dans la suite ordinaire ; PostgreSQL sous `TUNE_TEST_PG_URL`, pour
//! la requête stricte (`GROUP BY`, `SUM` en NUMERIC).

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::ecoutes_importees::EcoutesImporteesRepo;
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

fn executer(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

/// Sème la bibliothèque et les écoutes :
///
/// * « Ballade importée » : 40 écoutes importées, aucune dans l'historique ;
/// * « Moving » : 3 écoutes dans l'historique + 2 importées = 5 ;
/// * « Sinnerman » : 1 écoute dans l'historique, aucune importée.
///
/// Rend les écarts constatés.
async fn scenario(state: &AppState) -> Vec<String> {
    for sql in [
        "DELETE FROM listen_history",
        "DELETE FROM imported_plays",
        "DELETE FROM tracks WHERE id IN (777701, 777702)",
        "DELETE FROM albums WHERE id IN (777701, 777702)",
        "DELETE FROM artists WHERE id IN (777701, 777702)",
    ] {
        executer(state, sql);
    }
    for sql in [
        "INSERT INTO artists (id, name) VALUES (777701, 'Artiste importé')",
        "INSERT INTO artists (id, name) VALUES (777702, 'Kate Bush')",
        "INSERT INTO albums (id, title, artist_id) VALUES (777701, 'Album importé', 777701)",
        "INSERT INTO albums (id, title, artist_id) VALUES (777702, 'The Kick Inside', 777702)",
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format) \
         VALUES (777701, 'Ballade importée', 777701, 777701, '/m/7777/1.flac', 'flac')",
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format) \
         VALUES (777702, 'Moving', 777702, 777702, '/m/7777/2.flac', 'flac')",
    ] {
        executer(state, sql);
    }
    let quand = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    for (titre, artiste, album) in [
        ("Moving", "Kate Bush", "The Kick Inside"),
        ("Moving", "Kate Bush", "The Kick Inside"),
        ("Moving", "Kate Bush", "The Kick Inside"),
        ("Sinnerman", "Nina Simone", "Pastel Blues"),
    ] {
        executer(
            state,
            &format!(
                "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at, duration_ms) \
                 VALUES ('{titre}', '{artiste}', '{album}', 'local', '{quand}', 1000)"
            ),
        );
    }
    let importees = EcoutesImporteesRepo::with_backend(state.backend.clone());
    importees
        .ajouter(777701, 40, Some("2024-05-01T10:00:00Z"))
        .unwrap();
    importees.ajouter(777702, 2, None).unwrap();

    let mut ecarts = Vec::new();

    // --- « Titres les plus écoutés » (accueil, historique, tableau de bord) ---
    let top = corps(state, "/api/v1/history/top-tracks?limit=10").await;
    let lignes: Vec<(String, i64, Option<i64>)> = top
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|l| {
            (
                l["title"].as_str().unwrap_or_default().to_string(),
                l["plays"].as_i64().unwrap_or(-1),
                l["track_id"].as_i64(),
            )
        })
        .collect();
    let attendu = vec![
        ("Ballade importée".to_string(), 40, Some(777701)),
        ("Moving".to_string(), 5, Some(777702)),
        ("Sinnerman".to_string(), 1, None),
    ];
    if lignes != attendu {
        ecarts.push(format!(
            "top-tracks : écoutes importées ignorées — attendu {attendu:?}, obtenu {lignes:?}"
        ));
    }
    if top
        .as_array()
        .and_then(|a| a.first())
        .map(|l| l["album_title"] != "Album importé")
        .unwrap_or(true)
    {
        ecarts.push(format!("top-tracks : album du premier ({top})"));
    }

    // --- Statistiques d'écoute « tous temps » (Premium) ---
    state.license.set_account_premium(true, None).await;
    let stats = corps(state, "/api/v1/stats/listening?limit=10").await;
    let totaux = &stats["totals"];
    if totaux["total_listens"].as_i64() != Some(46) {
        ecarts.push(format!(
            "stats : total_listens = {} au lieu de 46 (4 dans l'historique + 42 importées)",
            totaux["total_listens"]
        ));
    }
    if totaux["imported_listens"].as_i64() != Some(42) {
        ecarts.push(format!(
            "stats : imported_listens = {} au lieu de 42",
            totaux["imported_listens"]
        ));
    }
    if totaux["unique_tracks"].as_i64() != Some(3) {
        ecarts.push(format!(
            "stats : unique_tracks = {} au lieu de 3",
            totaux["unique_tracks"]
        ));
    }
    // La durée écoutée reste celle de l'historique : une écoute importée n'en a pas.
    if totaux["total_duration_ms"].as_i64() != Some(4000) {
        ecarts.push(format!(
            "stats : total_duration_ms = {} au lieu de 4000",
            totaux["total_duration_ms"]
        ));
    }
    let premier = |cle: &str, nom: &str| -> (String, i64) {
        let l = &stats[cle][0];
        (
            l[nom].as_str().unwrap_or_default().to_string(),
            l["plays"].as_i64().unwrap_or(-1),
        )
    };
    for (cle, nom, attendu) in [
        ("top_tracks", "title", ("Ballade importée", 40)),
        ("top_artists", "artist", ("Artiste importé", 40)),
        ("top_albums", "album", ("Album importé", 40)),
    ] {
        let obtenu = premier(cle, nom);
        if obtenu != (attendu.0.to_string(), attendu.1) {
            ecarts.push(format!(
                "stats {cle} : premier = {obtenu:?}, attendu {attendu:?} ({})",
                stats[cle]
            ));
        }
    }
    // Kate Bush : 3 écoutes dans l'historique + 2 importées, un seul artiste.
    let kate: Vec<i64> = stats["top_artists"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|l| l["artist"] == "Kate Bush")
        .filter_map(|l| l["plays"].as_i64())
        .collect();
    if kate != vec![5] {
        ecarts.push(format!(
            "stats top_artists : Kate Bush = {kate:?}, attendu [5] ({})",
            stats["top_artists"]
        ));
    }
    if stats["imported_plays"]["all_time_only"] != true {
        ecarts.push(format!(
            "stats : imported_plays = {}",
            stats["imported_plays"]
        ));
    }
    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn ecoutes_importees_comptees_partout_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_ecoutes_importees_comptees_partout() {
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
