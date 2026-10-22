//! La règle « dans les N derniers jours » (`in_last`) des collections ET des
//! playlists intelligentes, sur une VRAIE base PostgreSQL (#6122, forum 2217).
//!
//! Les dates relatives existantes (« après now-90d ») passent par
//! `DATETIME('now', …)`, écriture propre à SQLite. La nouvelle règle compare
//! des LITTÉRAUX calculés à chaque requête — des secondes pour `file_mtime`,
//! de l'ISO 8601 pour `listened_at` — et c'est sur PostgreSQL que ce choix se
//! prouve. L'épreuve rejoue les deux aperçus par le vrai routeur. Même
//! doctrine que `pg_artiste_de_l_album_6121.rs` : variable ABSENTE ⇒ saut
//! annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes.
const MARQUE: &str = "date-relative-6122";

fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(state: &AppState) {
    let b = &state.backend;
    for sql in [
        format!("DELETE FROM listen_history WHERE title LIKE '{MARQUE}%'"),
        format!(
            "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

async fn apercu(state: &AppState, route: &str, regles: Value) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let corps = json!({"rules": regles, "match_mode": "all"});
    let requete = Request::builder()
        .method("POST")
        .uri(route)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    assert!(
        statut.is_success(),
        "POST {route} → {statut} sur PostgreSQL (#6122) : {texte}"
    );
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

fn titres(liste: &Value) -> Vec<String> {
    let mut t: Vec<String> = liste
        .as_array()
        .expect("une liste")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect();
    t.sort();
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6122_dans_les_n_derniers_jours_dans_les_collections_et_les_playlists() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let maintenant = chrono::Utc::now();
    let il_y_a = |j: i64| maintenant - chrono::Duration::days(j);

    let nom = format!("{MARQUE} Artiste");
    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste");
    let album = |titre: &str| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
            &[&titre as &dyn ToSqlValue, &artiste],
        )
        .expect("album")
    };
    let neuf = album("Neuf");
    let ancien = album("Ancien");
    let piste = |album_id: i64, titre: &str, mtime: f64| {
        let chemin = format!("/m/{MARQUE}/{titre}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, title, file_path, file_mtime) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                &album_id as &dyn ToSqlValue,
                &artiste,
                &titre,
                &chemin,
                &mtime,
            ],
        )
        .expect("piste")
    };
    // « Neuf » : un fichier de 5 jours, jamais écouté.
    piste(neuf, "Fraiche", il_y_a(5).timestamp() as f64);
    // « Ancien » : un fichier de 2019, écouté il y a 2 jours.
    let vieille = piste(ancien, "Vieille", 1_546_300_800.0);
    let ecoute = il_y_a(2).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let titre_ecoute = format!("{MARQUE} Vieille");
    b.execute(
        "INSERT INTO listen_history (track_id, title, listened_at) VALUES (?, ?, ?)",
        &[&vieille as &dyn ToSqlValue, &titre_ecoute, &ecoute],
    )
    .expect("écoute");

    // La règle testée, bornée à nos lignes (les tables sont partagées).
    let coll = "/api/v1/library/smart-collections/preview";
    let pl = "/api/v1/library/smart-playlists/preview";
    let regles = |champ: &str, borne: &str, n: Value| {
        json!([
            {"field": champ, "op": "in_last", "value": n},
            {"field": borne, "op": "contains", "value": MARQUE}
        ])
    };

    // 1. Collections — date d'ajout, puis dernière écoute.
    let r = apercu(&state, coll, regles("added_at", "title", json!(30))).await;
    assert_eq!(titres(&r["albums"]), vec![format!("{MARQUE} Neuf")], "{r}");
    let r = apercu(&state, coll, regles("added_at", "title", json!(3))).await;
    assert_eq!(r["total"], 0, "{r}");
    let r = apercu(&state, coll, regles("last_played_at", "title", json!("7"))).await;
    assert_eq!(
        titres(&r["albums"]),
        vec![format!("{MARQUE} Ancien")],
        "{r}"
    );
    let r = apercu(&state, coll, regles("last_played_at", "title", json!(1))).await;
    assert_eq!(r["total"], 0, "{r}");

    // 2. Playlists — les mêmes, à la piste.
    let r = apercu(&state, pl, regles("added_at", "album", json!(30))).await;
    assert_eq!(titres(&r["tracks"]), vec!["Fraiche".to_string()], "{r}");
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    let r = apercu(&state, pl, regles("last_played_at", "album", json!(7))).await;
    assert_eq!(titres(&r["tracks"]), vec!["Vieille".to_string()], "{r}");
    assert_eq!(r["regles_refusees"], json!([]), "{r}");

    // 3. Un N illisible ne retient rien, et la playlist le signale.
    let r = apercu(&state, coll, regles("added_at", "title", json!(0))).await;
    assert_eq!(r["total"], 0, "{r}");
    let r = apercu(&state, pl, regles("added_at", "album", json!("abc"))).await;
    assert_eq!(r["total"], 0, "{r}");
    assert_ne!(r["regles_refusees"], json!([]), "{r}");

    menage(&state);
}
