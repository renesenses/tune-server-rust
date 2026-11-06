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
//!
//! #7777 — second scénario, `scenario_ecoutes_importees` : les écoutes
//! IMPORTÉES (Roon, Plex, m3u ; table `imported_plays`, #7763) comptent dans
//! « Titres les plus écoutés » et les statistiques d'écoute tous temps
//! (`/stats/listening`), qui ne lisaient que `listen_history`. Ici parce que
//! ces requêtes ajoutent un `UNION ALL` et un `SUM` (NUMERIC sur PostgreSQL)
//! que seul PostgreSQL contrôle strictement.

use axum::body::Body;
use axum::http::Request;
use chrono::Datelike;
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
    // Le MÊME jour il y a deux ans : borner le jour à 28 semait l'écoute la
    // veille les 29, 30 et 31, et « Ce jour-là » revenait vide ces jours-là.
    // Un 29 février n'a pas d'homologue deux ans plus tôt : l'épreuve de
    // « Ce jour-là » est alors sautée.
    let jour_homologue = maintenant.date_naive().with_year(maintenant.year() - 2);
    let il_y_a_deux_ans = format!(
        "{}T12:00:00Z",
        jour_homologue
            .unwrap_or_else(|| maintenant.date_naive() - chrono::Days::new(730))
            .format("%Y-%m-%d")
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
    if jour_homologue.is_some() && titres != vec!["Wuthering Heights"] {
        ecarts.push(format!(
            "dashboard on_this_day : {titres:?} ({})",
            tableau["on_this_day"]
        ));
    }
    ecarts
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
async fn scenario_ecoutes_importees(state: &AppState) -> Vec<String> {
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
    // Le banc PostgreSQL est partagé, et `scenario` ne vide que
    // `listen_history` : les écoutes importées de « Moving » y
    // compteraient. On remet la base comme on l'a trouvée.
    for sql in [
        "DELETE FROM imported_plays",
        "DELETE FROM tracks WHERE id IN (777701, 777702)",
        "DELETE FROM albums WHERE id IN (777701, 777702)",
        "DELETE FROM artists WHERE id IN (777701, 777702)",
        "DELETE FROM listen_history",
    ] {
        executer(state, sql);
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

#[tokio::test(flavor = "multi_thread")]
async fn ecoutes_importees_comptees_partout_sur_sqlite_7777() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario_ecoutes_importees(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_ecoutes_importees_comptees_partout_7777() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    let ecarts = scenario_ecoutes_importees(&state).await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
