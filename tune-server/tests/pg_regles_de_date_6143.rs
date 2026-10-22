//! Les règles de DATE des collections ET des playlists intelligentes — « après »,
//! « avant », « entre », et les formes relatives (`now-90d`, `90d`) — sur une
//! VRAIE base PostgreSQL (#6143, relevé pendant #6122).
//!
//! Elles compilaient en `DATETIME(t.file_mtime, 'unixepoch')` et
//! `DATETIME('now', '-N days')`, deux écritures propres à SQLite : PostgreSQL
//! refuse la fonction, l'aperçu tombait en 500. Le correctif calcule la borne
//! côté Rust, comme `in_last` (#6141) : des secondes pour `file_mtime`, de
//! l'ISO 8601 pour `listened_at`. Un témoin par opérateur, chacun sur les deux
//! aperçus (collection, playlist) et les deux champs (date d'ajout, dernière
//! écoute). Même doctrine que `pg_date_relative_6122.rs` : variable ABSENTE ⇒
//! saut annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

const COLL: &str = "/api/v1/library/smart-collections/preview";
const PL: &str = "/api/v1/library/smart-playlists/preview";

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

/// Les tables sont partagées avec les autres bancs : on ne vide que les
/// lignes de SA marque (une par témoin).
fn menage(state: &AppState, marque: &str) {
    let b = &state.backend;
    for sql in [
        format!("DELETE FROM listen_history WHERE title LIKE '{marque}%'"),
        format!(
            "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{marque}%')"
        ),
        format!("DELETE FROM albums WHERE title LIKE '{marque}%'"),
        format!("DELETE FROM artists WHERE name LIKE '{marque}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

/// Trois albums, une piste chacun :
/// - « Neuf »   : fichier d'il y a 5 jours, jamais écouté ;
/// - « Ancien » : fichier du 01/01/2019, écouté il y a 2 jours ;
/// - « Oublie » : fichier du 15/06/2020, écouté le 01/03/2021 seulement.
fn semer(state: &AppState, marque: &str) {
    menage(state, marque);
    let b = state.backend.clone();
    let maintenant = chrono::Utc::now();
    let il_y_a = |j: i64| maintenant - chrono::Duration::days(j);

    let nom = format!("{marque} Artiste");
    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("artiste");
    let piste = |album: &str, titre: &str, mtime: f64, ecoute: Option<String>| {
        let titre_album = format!("{marque} {album}");
        let album_id = b
            .execute_returning_id(
                "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
                &[&titre_album as &dyn ToSqlValue, &artiste],
            )
            .expect("album");
        let chemin = format!("/m/{marque}/{titre}.flac");
        let id = b
            .execute_returning_id(
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
            .expect("piste");
        if let Some(quand) = ecoute {
            let titre_ecoute = format!("{marque} {titre}");
            b.execute(
                "INSERT INTO listen_history (track_id, title, listened_at) VALUES (?, ?, ?)",
                &[&id as &dyn ToSqlValue, &titre_ecoute, &quand],
            )
            .expect("écoute");
        }
    };
    piste("Neuf", "Fraiche", il_y_a(5).timestamp() as f64, None);
    piste(
        "Ancien",
        "Vieille",
        1_546_300_800.0,
        Some(il_y_a(2).format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    piste(
        "Oublie",
        "Delaissee",
        1_592_179_200.0,
        Some("2021-03-01T12:00:00Z".to_string()),
    );
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
        "POST {route} {regles} → {statut} sur PostgreSQL (#6143) : {texte}"
    );
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

fn titres(liste: &Value) -> Vec<String> {
    let mut t: Vec<String> = liste
        .as_array()
        .unwrap_or_else(|| panic!("une liste attendue : {liste}"))
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect();
    t.sort();
    t
}

/// Rejoue UNE règle de date sur les deux aperçus et compare les albums
/// (collection) puis les pistes (playlist) retenus.
async fn verifier(
    state: &AppState,
    marque: &str,
    champ: &str,
    op: &str,
    valeur: Value,
    albums: &[&str],
    pistes: &[&str],
) {
    let regle = |borne: &str| {
        json!([
            {"field": champ, "op": op, "value": valeur},
            {"field": borne, "op": "contains", "value": marque}
        ])
    };
    let r = apercu(state, COLL, regle("title")).await;
    let attendus: Vec<String> = albums.iter().map(|a| format!("{marque} {a}")).collect();
    assert_eq!(
        titres(&r["albums"]),
        attendus,
        "collection {champ} {op} {valeur} : {r}"
    );
    let r = apercu(state, PL, regle("album")).await;
    let attendues: Vec<String> = pistes.iter().map(|p| p.to_string()).collect();
    assert_eq!(
        titres(&r["tracks"]),
        attendues,
        "playlist {champ} {op} {valeur} : {r}"
    );
    assert_eq!(
        r["regles_refusees"],
        json!([]),
        "playlist {champ} {op} : {r}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6143_apres_une_date() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let m = "regles-date-6143-apres";
    let state = etat_postgres(&url);
    semer(&state, m);
    let d = json!("2022-01-01");
    verifier(
        &state,
        m,
        "added_at",
        ">",
        d.clone(),
        &["Neuf"],
        &["Fraiche"],
    )
    .await;
    verifier(
        &state,
        m,
        "last_played_at",
        ">",
        d,
        &["Ancien"],
        &["Vieille"],
    )
    .await;
    menage(&state, m);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6143_avant_une_date() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let m = "regles-date-6143-avant";
    let state = etat_postgres(&url);
    semer(&state, m);
    let d = json!("2022-01-01");
    verifier(
        &state,
        m,
        "added_at",
        "<",
        d.clone(),
        &["Ancien", "Oublie"],
        &["Delaissee", "Vieille"],
    )
    .await;
    verifier(
        &state,
        m,
        "last_played_at",
        "<",
        d,
        &["Oublie"],
        &["Delaissee"],
    )
    .await;
    menage(&state, m);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6143_entre_deux_dates() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let m = "regles-date-6143-entre";
    let state = etat_postgres(&url);
    semer(&state, m);
    verifier(
        &state,
        m,
        "added_at",
        "between",
        json!(["2018-06-01", "2020-01-01"]),
        &["Ancien"],
        &["Vieille"],
    )
    .await;
    verifier(
        &state,
        m,
        "last_played_at",
        "between",
        json!(["2021-01-01", "2021-12-31"]),
        &["Oublie"],
        &["Delaissee"],
    )
    .await;
    menage(&state, m);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6143_dates_relatives_now_moins_n_jours() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let m = "regles-date-6143-relatif";
    let state = etat_postgres(&url);
    semer(&state, m);
    // « now-90d » et la forme nue « 90d » de la collection « 🆕 Récents ».
    verifier(
        &state,
        m,
        "added_at",
        ">",
        json!("now-90d"),
        &["Neuf"],
        &["Fraiche"],
    )
    .await;
    verifier(
        &state,
        m,
        "added_at",
        ">",
        json!("90d"),
        &["Neuf"],
        &["Fraiche"],
    )
    .await;
    verifier(
        &state,
        m,
        "added_at",
        "between",
        json!(["now-30d", "now-1d"]),
        &["Neuf"],
        &["Fraiche"],
    )
    .await;
    verifier(
        &state,
        m,
        "last_played_at",
        ">",
        json!("now-90d"),
        &["Ancien"],
        &["Vieille"],
    )
    .await;
    verifier(
        &state,
        m,
        "last_played_at",
        "<",
        json!("now-90d"),
        &["Oublie"],
        &["Delaissee"],
    )
    .await;
    menage(&state, m);
}
