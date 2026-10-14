//! La règle « ne contient pas » des collections ET des playlists
//! intelligentes, sur une VRAIE base PostgreSQL (#6250).
//!
//! Le SQL est neuf des deux côtés : `(col IS NULL OR NOT (… LIKE …))` sur la
//! piste, et `NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = al.id …)`
//! pour un critère de piste dans une collection — une sous-requête qui
//! redéclare l'alias `t`. L'épreuve rejoue les deux aperçus par le vrai
//! routeur. Même doctrine que `pg_artiste_de_l_album_6121.rs` : variable
//! ABSENTE ⇒ saut annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes — et chaque règle est bornée
/// à nos albums, puisque « ne contient pas » retiendrait ceux des autres.
const MARQUE: &str = "ne-contient-pas-6250";

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
        "POST {route} → {statut} sur PostgreSQL (#6250) : {texte}"
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
async fn pg_6250_ne_contient_pas_dans_les_collections_et_les_playlists() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let artiste = |nom: &str| {
        let nom = format!("{MARQUE} {nom}");
        b.execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste")
    };
    let miles = artiste("Miles");
    let evans = artiste("Evans");
    let album = |titre: &str, label: Option<&str>| {
        let titre = format!("{MARQUE} {titre}");
        b.execute_returning_id(
            "INSERT INTO albums (title, artist_id, label) VALUES (?, ?, ?)",
            &[&titre as &dyn ToSqlValue, &miles, &label],
        )
        .expect("album")
    };
    let mixte = album("Mixte", Some("Columbia"));
    let jazz = album("Jazz", Some("Columbia"));
    let sans = album("Sans", None);
    let piste = |album_id: i64, artiste_id: i64, genre: Option<&str>, titre: &str| {
        let chemin = format!("/m/{MARQUE}/{titre}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, artist_id, genre, title, file_path) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                &album_id as &dyn ToSqlValue,
                &artiste_id,
                &genre,
                &titre,
                &chemin,
            ],
        )
        .expect("piste")
    };
    // Mixte : une piste Jazz ET une piste Rock. Sans : une piste sans genre,
    // d'un autre artiste.
    piste(mixte, miles, Some("Jazz"), "M-Jazz");
    piste(mixte, miles, Some("Rock"), "M-Rock");
    piste(jazz, miles, Some("Jazz"), "J-Jazz");
    piste(sans, evans, None, "S-Rien");

    let a = |t: &str| format!("{MARQUE} {t}");
    let nc = |champ: &str, v: &str| json!({"field": champ, "op": "not_contains", "value": v});

    // 1. Collections. Critère de PISTE : aucune piste de l'album ne contient.
    let coll = "/api/v1/library/smart-collections/preview";
    let borne = json!({"field": "title", "op": "contains", "value": MARQUE});
    let r = apercu(&state, coll, json!([borne, nc("genre", "ROCK")])).await;
    assert_eq!(titres(&r["albums"]), vec![a("Jazz"), a("Sans")], "{r}");
    // Critère d'ALBUM, valeur absente : « Sans » n'a pas de label.
    let r = apercu(&state, coll, json!([borne, nc("label", "colu")])).await;
    assert_eq!(titres(&r["albums"]), vec![a("Sans")], "{r}");

    // 2. Playlists : la piste elle-même ; la piste sans genre passe.
    let pl = "/api/v1/library/smart-playlists/preview";
    let borne = json!({"field": "album", "op": "contains", "value": MARQUE});
    let r = apercu(&state, pl, json!([borne, nc("genre", "rock")])).await;
    assert_eq!(
        titres(&r["tracks"]),
        vec![
            "J-Jazz".to_string(),
            "M-Jazz".to_string(),
            "S-Rien".to_string()
        ],
        "{r}"
    );
    assert_eq!(r["regles_refusees"], json!([]), "{r}");
    // Insensible aux accents, comme « contient ».
    let r = apercu(&state, pl, json!([borne, nc("artist", "MÎLES")])).await;
    assert_eq!(titres(&r["tracks"]), vec!["S-Rien".to_string()], "{r}");

    menage(&state);
}
