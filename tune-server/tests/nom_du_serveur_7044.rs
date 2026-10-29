//! #7044 — pouvoir NOMMER son serveur Tune.
//!
//! Le nom est affiché par la page publique des radios de mozaiklabs.fr
//! (« Connecté à Alec-server »). Le champ stable que cette page lit est
//! `server_name` dans `GET /api/v1/system/health` : route publique (aucun jeton,
//! même quand l'authentification est active), ouverte aux appels d'une autre
//! origine (CORS), toujours non vide. Il se règle par
//! `PATCH /api/v1/system/config {"server_name": "…"}` (administrateur).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET_JWT: &str = "test-jwt-secret";

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn avec_authentification(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET_JWT).unwrap();
}

fn admin() -> String {
    tune_server::auth::sign_jwt(1, "admin", SECRET_JWT).unwrap()
}

async fn envoyer(
    state: &AppState,
    methode: &str,
    chemin: &str,
    jeton: Option<&str>,
    origine: Option<&str>,
    corps: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut r = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        r = r.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    if let Some(o) = origine {
        r = r.header(header::ORIGIN, o);
    }
    let body = match corps {
        Some(c) => {
            r = r.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.oneshot(r.body(body).unwrap()).await.unwrap();
    let (status, en_tetes) = (rep.status(), rep.headers().clone());
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        en_tetes,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn nommer(state: &AppState, nom: Value) -> StatusCode {
    let j = admin();
    envoyer(
        state,
        "PATCH",
        "/api/v1/system/config",
        Some(&j),
        None,
        Some(json!({ "server_name": nom })),
    )
    .await
    .0
}

async fn nom_publie(state: &AppState) -> String {
    let (s, _, corps) = envoyer(
        state,
        "GET",
        "/api/v1/system/health",
        None,
        Some("https://mozaiklabs.fr"),
        None,
    )
    .await;
    assert!(s.is_success(), "health : {s}");
    corps["server_name"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn le_nom_choisi_est_lisible_sans_jeton_par_la_page_des_radios() {
    let state = etat();
    avec_authentification(&state);
    assert_eq!(nommer(&state, json!("Alec-server")).await, StatusCode::OK);

    // Sans jeton, depuis l'origine du site, authentification ACTIVE.
    let (s, en_tetes, corps) = envoyer(
        &state,
        "GET",
        "/api/v1/system/health",
        None,
        Some("https://mozaiklabs.fr"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "health doit rester public");
    assert_eq!(corps["server_name"], "Alec-server");
    assert!(
        en_tetes.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        "sans CORS la page mozaiklabs.fr ne peut pas lire le nom"
    );
}

#[tokio::test]
async fn sans_nom_choisi_le_champ_n_est_jamais_vide() {
    let state = etat();
    let nom = nom_publie(&state).await;
    assert!(!nom.trim().is_empty());

    // Effacer (vide, espaces, null) ramène au nom d'hôte, jamais à une étiquette vide.
    assert_eq!(nommer(&state, json!("Salon")).await, StatusCode::OK);
    assert_eq!(nom_publie(&state).await, "Salon");
    for vide in [json!(""), json!("   "), json!(null)] {
        assert_eq!(nommer(&state, json!("Salon")).await, StatusCode::OK);
        assert_eq!(nommer(&state, vide).await, StatusCode::OK);
        assert_eq!(nom_publie(&state).await, nom, "retour au nom d'hôte");
    }
}

#[tokio::test]
async fn un_nom_inaffichable_est_refuse_et_l_ancien_reste() {
    let state = etat();
    assert_eq!(nommer(&state, json!("Salon")).await, StatusCode::OK);

    let trop_long = "x".repeat(65);
    for (mauvais, raison) in [
        (json!(trop_long), "65 caractères"),
        (json!("Salon\nCuisine"), "retour à la ligne"),
        (json!("Sa\tlon"), "tabulation"),
        (json!("Sa\u{0}lon"), "NUL"),
        (json!(42), "pas un texte"),
    ] {
        assert_eq!(
            nommer(&state, mauvais).await,
            StatusCode::BAD_REQUEST,
            "{raison} doit être refusé"
        );
        assert_eq!(
            nom_publie(&state).await,
            "Salon",
            "{raison} : l'ancien reste"
        );
    }
    // Les bornes sont acceptées : 64 caractères, accents et espaces intérieurs.
    assert_eq!(nommer(&state, json!("é".repeat(64))).await, StatusCode::OK);
    assert_eq!(
        nommer(&state, json!("  Salle de musique  ")).await,
        StatusCode::OK
    );
    assert_eq!(nom_publie(&state).await, "Salle de musique");
}

#[tokio::test]
async fn seul_un_administrateur_peut_nommer_le_serveur() {
    let state = etat();
    avec_authentification(&state);
    let (s, _, _) = envoyer(
        &state,
        "PATCH",
        "/api/v1/system/config",
        None,
        None,
        Some(json!({ "server_name": "Intrus" })),
    )
    .await;
    assert!(
        s == StatusCode::UNAUTHORIZED || s == StatusCode::FORBIDDEN,
        "sans jeton : {s}"
    );
    assert_ne!(nom_publie(&state).await, "Intrus");
}
