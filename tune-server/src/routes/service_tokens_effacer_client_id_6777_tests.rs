//! #6777 (part serveur de tune-web-client#2304, fils 2384 et 2385, rc7
//! Windows) — un Client ID Spotify erroné ne pouvait être ni corrigé ni
//! effacé depuis l'écran ; la seule route d'effacement emportait toutes les
//! clés `spotify_*`.
//!
//! Témoins sur les ROUTES MONTÉES, devant le vrai `SpotifyService` (aucun
//! réseau). Le Client ID est factice.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

use super::source_du_client_id_spotify;

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

async fn app() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let svc = state.services.lock().await.get("spotify").unwrap();
    svc.write()
        .await
        .as_any_mut()
        .downcast_mut::<tune_core::streaming::spotify::SpotifyService>()
        .unwrap()
        .set_client_id("placeholder");
    (crate::routes::router(state.clone()), state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json")
        .body(match corps {
            Some(c) => Body::from(c.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let brut = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&brut).unwrap_or(Value::Null))
}

fn ligne_spotify(liste: &Value) -> Value {
    liste
        .as_array()
        .or_else(|| liste["services"].as_array())
        .and_then(|l| l.iter().find(|s| s["id"] == "spotify").cloned())
        .unwrap_or(Value::Null)
}

#[test]
fn la_source_suit_l_ordre_du_demarrage_6777() {
    let autre = "fedcba9876543210fedcba9876543210";
    assert_eq!(
        source_du_client_id_spotify(Some(autre), Some(CLIENT_ID), None, None),
        Some("env")
    );
    assert_eq!(
        source_du_client_id_spotify(None, Some(CLIENT_ID), Some(autre), None),
        Some("db")
    );
    assert_eq!(
        source_du_client_id_spotify(None, None, Some(autre), None),
        Some("config")
    );
    assert_eq!(
        source_du_client_id_spotify(None, None, None, Some(autre)),
        Some("env")
    );
    assert_eq!(
        source_du_client_id_spotify(None, Some("placeholder"), Some(" "), None),
        None
    );
}

#[tokio::test]
async fn effacer_le_client_id_ne_touche_a_aucun_autre_reglage_spotify_6777() {
    if std::env::var("TUNE_SPOTIFY_CLIENT_ID").is_ok() || std::env::var("SPOTIFY_CLIENT_ID").is_ok()
    {
        return; // la variable l'emporte par construction
    }
    let (app, state) = app().await;
    let (s, rep) = appel(
        &app,
        "POST",
        "/api/v1/services/tokens/spotify",
        Some(json!({ "client_id": CLIENT_ID })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("spotify_connect_enabled", "true").unwrap();
    settings
        .set("spotify_connect_custom_name", "Salon")
        .unwrap();
    settings.set("spotify_playback_disabled", "false").unwrap();

    // La ligne dit que le Client ID vient de la base : l'écran peut l'effacer.
    let (_, liste) = appel(&app, "GET", "/api/v1/services/tokens", None).await;
    let ligne = ligne_spotify(&liste);
    assert_eq!(ligne["spotify_client_id_source"], "db", "{ligne}");
    assert_eq!(ligne["spotify_client_id_configure"], true, "{ligne}");

    let (s, rep) = appel(
        &app,
        "DELETE",
        "/api/v1/services/tokens/spotify/client-id",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    assert_eq!(rep["spotify_client_id_configure"], false, "{rep}");
    assert!(rep["spotify_client_id_source"].is_null(), "{rep}");

    assert!(settings.get("spotify_client_id").unwrap().is_none());
    assert_eq!(
        settings.get("spotify_connect_enabled").unwrap().as_deref(),
        Some("true"),
        "#6777 : effacer le Client ID a emporté les réglages de Spotify Connect"
    );
    assert_eq!(
        settings
            .get("spotify_connect_custom_name")
            .unwrap()
            .as_deref(),
        Some("Salon")
    );
    assert_eq!(
        settings
            .get("spotify_playback_disabled")
            .unwrap()
            .as_deref(),
        Some("false")
    );

    // Le service est revenu à chaud au repli : l'écran rouvre le champ.
    let (_, liste) = appel(&app, "GET", "/api/v1/services/tokens", None).await;
    let ligne = ligne_spotify(&liste);
    assert_eq!(ligne["spotify_client_id_configure"], false, "{ligne}");
    assert!(ligne["spotify_client_id_source"].is_null(), "{ligne}");
}

#[tokio::test]
async fn la_route_n_existe_que_pour_spotify_et_le_test_de_spotify_reste_route_6777() {
    let (app, _state) = app().await;
    let (s, _) = appel(
        &app,
        "DELETE",
        "/api/v1/services/tokens/deezer/client-id",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = appel(&app, "POST", "/api/v1/services/tokens/spotify/test", None).await;
    assert_ne!(
        s,
        StatusCode::NOT_FOUND,
        "la route de test de Spotify doit rester montée"
    );
    assert_ne!(s, StatusCode::METHOD_NOT_ALLOWED);
}
