//! Contrôle parental, étape 1 (#7856), de bout en bout par l'API.
//!
//! - sans politique, un profil reçoit exactement les mêmes listes ;
//! - un artiste bloqué disparaît des albums, des artistes, des pistes et de
//!   la recherche du profil supervisé, pas de ceux des autres profils ;
//! - une radio ou un podcast hors liste blanche disparaît de ses listes ;
//! - le profil supervisé ne peut ni lire, ni modifier, ni retirer sa
//!   politique (403) ;
//! - une révision périmée est refusée (409).
//!
//! Le même contrat est éprouvé sur PostgreSQL au niveau du stockage par
//! `tune_core::db::politique_parentale` (`pg_7856_*`).

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

/// Les listes gardées, telles qu'un profil les reçoit.
const LISTES: [&str; 7] = [
    "/api/v1/library/albums",
    "/api/v1/library/artists",
    "/api/v1/library/tracks",
    "/api/v1/library/search?q=Groupe",
    "/api/v1/radios",
    "/api/v1/radios/search?q=Radio",
    "/api/v1/podcasts/subscriptions",
];

async fn listes(app: &axum::Router, profil: i64) -> Vec<Value> {
    let mut sortie = Vec::new();
    for chemin in LISTES {
        let (statut, corps) = appel(app, "GET", chemin, profil, None).await;
        assert!(statut.is_success(), "{chemin} : {statut} {corps}");
        sortie.push(corps);
    }
    sortie
}

fn contient(corps: &Value, aiguille: &str) -> bool {
    corps.to_string().contains(aiguille)
}

fn politique() -> Value {
    json!({
        "artistes_bloques": [{"id": 7, "nom": "Groupe Interdit"}],
        "radios_autorisees": [RADIO_PERMISE],
        "podcasts_autorises": ["https://rss.example/permis.xml"],
        "plages_horaires": [{"jours": [6, 7], "debut": "09:00", "fin": "18:00"}],
        "budget_quotidien_minutes": 60
    })
}

#[tokio::test]
async fn sans_politique_les_listes_ne_changent_pas_7856() {
    let (app, _) = app();
    let admin = listes(&app, ADMIN).await;
    let enfant = listes(&app, ENFANT).await;
    assert_eq!(
        admin, enfant,
        "sans politique, le profil reçoit les mêmes listes"
    );
    // Et les témoins sont bien là : sans eux, l'égalité ne prouverait rien.
    assert!(
        contient(&enfant[0], "Album de Groupe Interdit"),
        "{}",
        enfant[0]
    );
    assert!(contient(&enfant[1], "Groupe Interdit"), "{}", enfant[1]);
    assert!(contient(&enfant[4], "Radio hors liste"), "{}", enfant[4]);
    assert!(contient(&enfant[6], "autre.xml"), "{}", enfant[6]);
}

#[tokio::test]
async fn le_profil_supervise_ne_voit_plus_ce_qui_est_bloque_7856() {
    let (app, _) = app();
    let (statut, corps) = appel(
        &app,
        "PUT",
        "/api/v1/profiles/2/politique-parentale",
        ADMIN,
        Some(json!({"revision": 0, "politique": politique()})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["revision"], 1);
    assert_eq!(corps["superviseur_id"], ADMIN);

    let enfant = listes(&app, ENFANT).await;
    for (chemin, corps) in LISTES.iter().zip(&enfant) {
        assert!(
            !contient(corps, "Groupe Interdit"),
            "artiste bloqué encore servi au profil supervisé par {chemin} : {corps}"
        );
    }
    assert!(
        contient(&enfant[0], "Album de Groupe Libre"),
        "{}",
        enfant[0]
    );
    assert!(
        contient(&enfant[2], "Titre de Groupe Libre"),
        "{}",
        enfant[2]
    );
    assert!(
        !contient(&enfant[4], "Radio hors liste"),
        "radio hors liste blanche servie : {}",
        enfant[4]
    );
    assert!(contient(&enfant[4], "Radio permise"), "{}", enfant[4]);
    assert!(!contient(&enfant[5], "Radio hors liste"), "{}", enfant[5]);
    assert!(
        !contient(&enfant[6], "autre.xml"),
        "podcast hors liste blanche servi : {}",
        enfant[6]
    );
    assert!(contient(&enfant[6], "permis.xml"), "{}", enfant[6]);

    // Les autres profils ne sont pas touchés.
    let admin = listes(&app, ADMIN).await;
    assert!(
        contient(&admin[0], "Album de Groupe Interdit"),
        "{}",
        admin[0]
    );
    assert!(contient(&admin[4], "Radio hors liste"), "{}", admin[4]);
}

#[tokio::test]
async fn le_profil_supervise_n_a_aucun_droit_sur_sa_politique_7856() {
    let (app, _) = app();
    let chemin = "/api/v1/profiles/2/politique-parentale";
    // Avant qu'une politique existe, il ne peut pas se la poser.
    let (statut, _) = appel(
        &app,
        "PUT",
        chemin,
        ENFANT,
        Some(json!({"revision": 0, "politique": {}})),
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
    let (statut, _) = appel(
        &app,
        "PUT",
        chemin,
        ADMIN,
        Some(json!({"revision": 0, "politique": politique()})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);

    let (statut, corps) = appel(&app, "GET", chemin, ENFANT, None).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "lecture par le profil supervisé : {corps}"
    );
    assert_eq!(
        corps["error"],
        "politique_parentale_reservee_au_superviseur"
    );
    let (statut, _) = appel(
        &app,
        "PUT",
        chemin,
        ENFANT,
        Some(json!({"revision": 1, "politique": {}})),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "écriture par le profil supervisé"
    );
    let (statut, _) = appel(
        &app,
        "DELETE",
        &format!("{chemin}?revision=1"),
        ENFANT,
        None,
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "retrait par le profil supervisé"
    );

    // La politique est intacte, et le superviseur la lit.
    let (statut, corps) = appel(&app, "GET", chemin, ADMIN, None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["revision"], 1);
    assert_eq!(corps["politique"]["budget_quotidien_minutes"], 60);
}

#[tokio::test]
async fn une_revision_perimee_est_refusee_7856() {
    let (app, _) = app();
    let chemin = "/api/v1/profiles/2/politique-parentale";
    let (statut, _) = appel(&app, "GET", chemin, ADMIN, None).await;
    assert_eq!(statut, StatusCode::NOT_FOUND, "aucune politique par défaut");
    let (statut, _) = appel(
        &app,
        "PUT",
        chemin,
        ADMIN,
        Some(json!({"revision": 0, "politique": politique()})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let (statut, _) = appel(
        &app,
        "PUT",
        chemin,
        ADMIN,
        Some(json!({"revision": 0, "politique": {}})),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "seconde création");
    let (statut, corps) = appel(
        &app,
        "PUT",
        chemin,
        ADMIN,
        Some(json!({"revision": 1, "politique": {}})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["revision"], 2);
    let (statut, corps) = appel(
        &app,
        "PUT",
        chemin,
        ADMIN,
        Some(json!({"revision": 1, "politique": politique()})),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::CONFLICT,
        "révision périmée acceptée : {corps}"
    );
    assert_eq!(corps["error"], "revision_perimee");
    let (statut, _) = appel(&app, "DELETE", &format!("{chemin}?revision=1"), ADMIN, None).await;
    assert_eq!(
        statut,
        StatusCode::CONFLICT,
        "retrait à une révision périmée"
    );
    // Une politique invalide est refusée avant toute écriture.
    let (statut, _) = appel(
        &app,
        "PUT",
        chemin,
        ADMIN,
        Some(json!({"revision": 2, "politique": {"plages_horaires": [{"jours": [9], "debut": "08:00", "fin": "09:00"}]}})),
    )
    .await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    let (statut, _) = appel(&app, "DELETE", &format!("{chemin}?revision=2"), ADMIN, None).await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert_eq!(
        listes(&app, ENFANT).await,
        listes(&app, ADMIN).await,
        "retirée, la politique ne filtre plus"
    );
}
