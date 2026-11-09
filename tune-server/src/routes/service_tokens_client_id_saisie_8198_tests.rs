//! renesenses/tune-server-rust#8198 — la saisie du Client ID Spotify.
//!
//! Vu en direct avec un testeur : Spotify répondait « INVALID_CLIENT: Invalid
//! client » parce que le champ contenait l'adresse du tableau de bord, pas le
//! Client ID. `POST /api/v1/services/tokens/spotify` n'accepte plus que
//! 32 caractères hexadécimaux, et tire l'identifiant d'une adresse
//! `developer.spotify.com/dashboard/<id>` en le disant.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

async fn app() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let svc = state.services.lock().await.get("spotify").unwrap();
        svc.write()
            .await
            .as_any_mut()
            .downcast_mut::<tune_core::streaming::spotify::SpotifyService>()
            .unwrap()
            .set_client_id("placeholder");
    }
    (crate::routes::router(state.clone()), state)
}

async fn poser(app: &axum::Router, saisie: &str) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/services/tokens/spotify")
        .header("content-type", "application/json")
        .header("accept-language", "fr")
        .body(Body::from(json!({ "client_id": saisie }).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let brut = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&brut).unwrap()
}

fn enregistre(state: &crate::state::AppState) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get("spotify_client_id")
        .unwrap()
}

#[tokio::test]
async fn trente_deux_hexadecimaux_sont_acceptes_tels_quels() {
    if std::env::var("TUNE_SPOTIFY_CLIENT_ID").is_ok() {
        return; // la variable l'emporte par construction
    }
    let (app, state) = app().await;
    let rep = poser(&app, &format!("  {CLIENT_ID}\n")).await;
    assert_eq!(rep["valid"], true, "{rep}");
    assert_eq!(rep["client_id_extrait"], false, "{rep}");
    assert_eq!(enregistre(&state).as_deref(), Some(CLIENT_ID));
}

#[tokio::test]
async fn l_adresse_du_tableau_de_bord_donne_son_client_id() {
    if std::env::var("TUNE_SPOTIFY_CLIENT_ID").is_ok() {
        return;
    }
    for adresse in [
        format!("https://developer.spotify.com/dashboard/{CLIENT_ID}"),
        format!("https://developer.spotify.com/dashboard/{CLIENT_ID}/settings"),
        format!("developer.spotify.com/dashboard/{CLIENT_ID}?tab=users"),
        format!(" HTTPS://Developer.Spotify.com/dashboard/{CLIENT_ID}/users "),
    ] {
        let (app, state) = app().await;
        let rep = poser(&app, &adresse).await;
        assert_eq!(rep["valid"], true, "{adresse} : {rep}");
        assert_eq!(rep["client_id_extrait"], true, "{adresse} : {rep}");
        assert!(
            rep["validation_message"]
                .as_str()
                .unwrap()
                .contains("tableau de bord"),
            "le message dit qu'il a été extrait : {rep}"
        );
        assert_eq!(enregistre(&state).as_deref(), Some(CLIENT_ID), "{adresse}");
    }
}

#[tokio::test]
async fn ce_qui_n_est_pas_un_client_id_est_refuse_avec_le_geste() {
    let (app, state) = app().await;
    for mauvais in [
        // Le cas du testeur : l'adresse du tableau de bord, sans identifiant.
        "https://developer.spotify.com/dashboard",
        "https://developer.spotify.com/dashboard/create",
        // Longueur fausse, caractères non hexadécimaux.
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdefghij456789abcdef",
        // Une autre adresse, même porteuse de 32 hexadécimaux.
        "https://example.com/dashboard/0123456789abcdef0123456789abcdef",
        "http://127.0.0.1:8888/api/v1/streaming/spotify/callback?code=abc",
    ] {
        let rep = poser(&app, mauvais).await;
        assert_eq!(rep["valid"], false, "{mauvais} : {rep}");
        assert_eq!(
            rep["code"], "spotify_client_id_invalide",
            "{mauvais} : {rep}"
        );
        let message = rep["validation_message"].as_str().unwrap();
        assert!(
            message.contains("Settings › Basic Information"),
            "le refus dit où le copier : {message}"
        );
    }
    assert!(enregistre(&state).is_none());
}
