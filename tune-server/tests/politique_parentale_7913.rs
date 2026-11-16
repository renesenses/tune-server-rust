//! #7913 — un artiste bloqué n'apparaît plus nulle part pour un profil
//! supervisé (suite de l'étape 1, #7856), de bout en bout par l'API :
//!
//! - les rangées de l'accueil et les sous-listes de la bibliothèque le
//!   retirent ;
//! - ouvrir directement la fiche d'un album, d'un artiste, d'une piste ou
//!   d'une radio bloqués rend 403 `parental_content_blocked` ;
//! - le total d'une liste paginée compte seulement ce que le profil voit,
//!   même quand l'élément bloqué est sur une AUTRE page ;
//! - `PUT /profiles/{id}` ne fait pas d'un profil supervisé un administrateur.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const RADIO_PERMISE: &str = "https://flux.example/permise";

fn app() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let db = &state.backend;
    let sql = |s: &str, p: &[&dyn ToSqlValue]| {
        db.execute(s, p).unwrap();
    };
    sql(
        "INSERT INTO profiles (id, username, display_name, is_admin) VALUES (2, 'enfant', 'Enfant', 0)",
        &[],
    );
    for (id, nom) in [(7i64, "Groupe Interdit"), (8, "Groupe Libre")] {
        sql("INSERT INTO artists (id, name) VALUES (?, ?)", &[&id, &nom]);
        sql(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[&(id * 10), &format!("Album de {nom}"), &id],
        );
        sql(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format) VALUES (?, ?, ?, ?, ?, 'flac')",
            &[
                &(id * 100),
                &format!("Titre de {nom}"),
                &id,
                &(id * 10),
                &format!("/musique/{id}.flac"),
            ],
        );
    }
    for (nom, url) in [
        ("Radio permise", RADIO_PERMISE),
        ("Radio hors liste", "https://flux.example/autre"),
    ] {
        sql(
            "INSERT INTO radio_stations (name, url) VALUES (?, ?)",
            &[&nom, &url],
        );
    }
    for (titre, flux) in [
        ("Permis", "https://rss.example/permis.xml"),
        ("Autre", "https://rss.example/autre.xml"),
    ] {
        sql(
            "INSERT INTO podcast_subscriptions (feed_url, title) VALUES (?, ?)",
            &[&flux, &titre],
        );
    }
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: i64,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("X-Profile-Id", profil.to_string())
        .header("content-type", "application/json");
    let req = req
        .body(
            corps
                .map(|c| Body::from(c.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let reponse = app.clone().oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn superviser(app: &axum::Router) {
    let (statut, corps) = appel(
        app,
        "PUT",
        "/api/v1/profiles/2/politique-parentale",
        ADMIN,
        Some(json!({"revision": 0, "politique": {
            "artistes_bloques": [{"id": 7, "nom": "Groupe Interdit"}],
            "radios_autorisees": [RADIO_PERMISE],
        }})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
}

fn contient(corps: &Value, aiguille: &str) -> bool {
    corps.to_string().contains(aiguille)
}

#[tokio::test]
async fn la_fiche_d_un_contenu_bloque_est_refusee_7913() {
    let (app, state) = app();
    superviser(&app).await;
    let radio_hors_liste = state
        .backend
        .query_one(
            "SELECT id FROM radio_stations WHERE name = 'Radio hors liste'",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap();
    for chemin in [
        "/api/v1/library/albums/70".to_string(),
        "/api/v1/library/artists/7".to_string(),
        "/api/v1/library/tracks/700".to_string(),
        format!("/api/v1/radios/{radio_hors_liste}"),
    ] {
        // L'administrateur l'ouvre : le témoin n'est pas vide de sens.
        let (statut, corps) = appel(&app, "GET", &chemin, ADMIN, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} (admin) : {corps}");
        let (statut, corps) = appel(&app, "GET", &chemin, ENFANT, None).await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "#7913 : fiche bloquée ouverte directement par le profil supervisé, {chemin} : {corps}"
        );
        assert_eq!(corps["error"], "parental_content_blocked", "{corps}");
    }
    for chemin in ["/api/v1/library/albums/80", "/api/v1/library/artists/8"] {
        let (statut, corps) = appel(&app, "GET", chemin, ENFANT, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} : {corps}");
    }
}

#[tokio::test]
async fn le_total_compte_aussi_les_autres_pages_7913() {
    let (app, _) = app();
    superviser(&app).await;
    // Tri par titre : « Album de Groupe Interdit » est en page 1, « Album de
    // Groupe Libre » en page 2. La page 2 ne contient rien de bloqué ; avant,
    // son total restait 2.
    let chemin = "/api/v1/library/albums?sort=title&order=asc&limit=1&offset=1";
    let (statut, admin) = appel(&app, "GET", chemin, ADMIN, None).await;
    assert_eq!(statut, StatusCode::OK, "{admin}");
    assert_eq!(
        admin["total"], 2,
        "banc : deux albums pour l'administrateur : {admin}"
    );
    let (statut, enfant) = appel(&app, "GET", chemin, ENFANT, None).await;
    assert_eq!(statut, StatusCode::OK, "{enfant}");
    assert!(contient(&enfant, "Album de Groupe Libre"), "{enfant}");
    assert_eq!(
        enfant["total"], 1,
        "#7913 : le total compte un album bloqué servi sur une autre page : {enfant}"
    );

    let (_, artistes) = appel(
        &app,
        "GET",
        "/api/v1/library/artists?limit=1&offset=5",
        ENFANT,
        None,
    )
    .await;
    assert_eq!(artistes["total"], 1, "#7913 : artistes : {artistes}");
}

#[tokio::test]
async fn les_sous_listes_et_l_accueil_le_retirent_7913() {
    let (app, state) = app();
    superviser(&app).await;
    // « Ajouts récents » lit la date du fichier : les pistes du banc sont neuves.
    state
        .backend
        .execute(
            "UPDATE tracks SET file_mtime = CAST(strftime('%s', 'now') AS REAL)",
            &[],
        )
        .unwrap();
    for chemin in [
        "/api/v1/home/recently-added?days=730",
        "/api/v1/library/albums/70/tracks",
        "/api/v1/library/artists/7/albums",
    ] {
        let (statut, admin) = appel(&app, "GET", chemin, ADMIN, None).await;
        assert!(statut.is_success(), "{chemin} : {statut} {admin}");
        assert!(
            contient(&admin, "Groupe Interdit"),
            "banc : l'administrateur doit le voir par {chemin} : {admin}"
        );
        let (statut, enfant) = appel(&app, "GET", chemin, ENFANT, None).await;
        assert!(statut.is_success(), "{chemin} : {statut} {enfant}");
        assert!(
            !contient(&enfant, "Groupe Interdit"),
            "#7913 : artiste bloqué servi au profil supervisé par {chemin} : {enfant}"
        );
    }
}

#[tokio::test]
async fn un_profil_supervise_ne_devient_pas_administrateur_7913() {
    let (app, state) = app();
    superviser(&app).await;
    let (statut, corps) = appel(
        &app,
        "PUT",
        "/api/v1/profiles/2",
        ADMIN,
        Some(json!({"name": "Enfant", "is_admin": true, "role": "admin"})),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert!(
        !tune_core::db::politique_parentale::est_administrateur(&state.backend, ENFANT).unwrap(),
        "#7913 : un profil supervisé ne reçoit pas le rôle administrateur"
    );
}
