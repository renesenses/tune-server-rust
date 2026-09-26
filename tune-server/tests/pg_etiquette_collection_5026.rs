//! La règle « Étiquette » élargie d'une collection intelligente, sur une VRAIE
//! base PostgreSQL (#5026).
//!
//! Sevy Tabroc, fil 1937 (0.9.164) : « Sept Oct 2026 (1) » dans le sélecteur,
//! « 0 albums correspondent » dans l'aperçu. La règle ne lisait que les albums
//! et les artistes de `item_tags` ; elle fait désormais entrer l'album dont
//! AU MOINS UNE piste porte l'étiquette, et l'album de SERVICE étiqueté
//! (`streaming_item_tags`).
//!
//! Le SQL est neuf des deux côtés : sous-requête sur `tracks`, `GROUP BY` sur
//! la paire `source` + `source_id`, `COUNT(*)` sur une table dérivée,
//! `EXISTS` corrélés. SQLite en accepte plus que PostgreSQL (colonne ni
//! groupée ni agrégée, table dérivée sans alias) : l'épreuve rejoue les trois
//! routes par le vrai routeur. Même doctrine que `pg_routes_serveur.rs` :
//! variable ABSENTE ⇒ saut annoncé ; variable POSÉE mais injoignable ⇒ le test
//! TOMBE.

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
const MARQUE: &str = "etiquette-5026";

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
            "DELETE FROM item_tags WHERE tag_id IN (SELECT id FROM tags WHERE name = '{MARQUE}')"
        ),
        format!(
            "DELETE FROM streaming_item_tags WHERE tag_id IN (SELECT id FROM tags WHERE name = '{MARQUE}')"
        ),
        format!("DELETE FROM tags WHERE name = '{MARQUE}'"),
        format!("DELETE FROM smart_collections WHERE name = '{MARQUE}'"),
        format!(
            "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM albums WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name = '{MARQUE}'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

async fn appel(state: &AppState, methode: &str, route: &str, corps: Option<Value>) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let requete = Request::builder()
        .method(methode)
        .uri(route)
        .header(header::CONTENT_TYPE, "application/json");
    let requete = match corps {
        Some(c) => requete.body(Body::from(c.to_string())).unwrap(),
        None => requete.body(Body::empty()).unwrap(),
    };
    let reponse = app.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let corps = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&corps).to_string();
    assert!(
        statut.is_success(),
        "{methode} {route} → {statut} sur PostgreSQL (#5026) : {texte}"
    );
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

fn titres(albums: &Value) -> Vec<String> {
    albums
        .as_array()
        .expect("une liste d'albums")
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_5026_la_regle_etiquette_lit_les_pistes_et_les_albums_de_service() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let artiste = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&MARQUE as &dyn ToSqlValue],
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
    let local = album("local");
    let par_piste = album("par piste");
    let rien = album("rien");
    let piste = |album_id: i64, n: &str| {
        let chemin = format!("/m/{MARQUE}/{n}.flac");
        b.execute_returning_id(
            "INSERT INTO tracks (album_id, title, file_path) VALUES (?, ?, ?)",
            &[&album_id as &dyn ToSqlValue, &n, &chemin],
        )
        .expect("piste")
    };
    piste(local, "a");
    let etiquetee = piste(par_piste, "b");
    piste(par_piste, "c");
    piste(rien, "d");
    let tag = b
        .execute_returning_id(
            "INSERT INTO tags (name) VALUES (?)",
            &[&MARQUE as &dyn ToSqlValue],
        )
        .expect("étiquette");
    for (type_, id) in [("album", local), ("track", etiquetee)] {
        b.execute(
            "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (?, ?, ?)",
            &[&tag as &dyn ToSqlValue, &type_, &id],
        )
        .expect("item_tags");
    }
    let service_id = format!("{MARQUE}-q1");
    let titre_service = format!("{MARQUE} service");
    b.execute(
        "INSERT INTO streaming_item_tags \
         (tag_id, item_type, source, source_id, title, artist, cover_url, created_at) \
         VALUES (?, 'album', 'qobuz', ?, ?, 'Artiste Q', 'https://c/q1.jpg', '2026-09-25T15:00:00Z')",
        &[&tag as &dyn ToSqlValue, &service_id, &titre_service],
    )
    .expect("streaming_item_tags");

    let porte = json!([{"field": "tag", "op": "is", "value": tag.to_string()}]);

    // 1. L'aperçu — la requête exacte du sélecteur web (web#1607).
    let apercu = appel(
        &state,
        "POST",
        "/api/v1/library/smart-collections/preview",
        Some(json!({"rules": porte, "match_mode": "all"})),
    )
    .await;
    assert_eq!(
        titres(&apercu["albums"]),
        vec![
            format!("{MARQUE} local"),
            format!("{MARQUE} par piste"),
            titre_service.clone()
        ],
        "{apercu}"
    );
    assert_eq!(apercu["total"], 3, "{apercu}");
    assert_eq!(apercu["albums"][2]["source"], "qobuz", "{apercu}");
    assert_eq!(
        apercu["albums"][2]["source_id"],
        service_id.as_str(),
        "{apercu}"
    );

    // 2. La négation : ni l'album, ni AUCUNE de ses pistes.
    let sans = appel(
        &state,
        "POST",
        "/api/v1/library/smart-collections/preview",
        Some(json!({
            "rules": [{"field": "tag", "op": "is_not", "value": tag.to_string()}],
            "match_mode": "all"
        })),
    )
    .await;
    let sans = titres(&sans["albums"]);
    assert!(sans.contains(&format!("{MARQUE} rien")), "{sans:?}");
    assert!(!sans.contains(&format!("{MARQUE} local")), "{sans:?}");
    assert!(!sans.contains(&format!("{MARQUE} par piste")), "{sans:?}");
    assert!(!sans.contains(&titre_service), "{sans:?}");

    // 3. La collection enregistrée, et le compteur de la liste.
    let creee = appel(
        &state,
        "POST",
        "/api/v1/library/smart-collections",
        Some(json!({"name": MARQUE, "rules": porte, "match_mode": "all", "sort_by": "title"})),
    )
    .await;
    let id = creee["id"].as_i64().expect("id de la collection");
    let albums = appel(
        &state,
        "GET",
        &format!("/api/v1/library/smart-collections/{id}/albums"),
        None,
    )
    .await;
    assert_eq!(titres(&albums).len(), 3, "{albums}");
    let liste = appel(&state, "GET", "/api/v1/library/smart-collections", None).await;
    let col = liste
        .as_array()
        .expect("liste")
        .iter()
        .find(|c| c["id"].as_i64() == Some(id))
        .cloned()
        .expect("la collection est listée");
    assert_eq!(col["album_count"], 3, "{col}");

    menage(&state);
}
