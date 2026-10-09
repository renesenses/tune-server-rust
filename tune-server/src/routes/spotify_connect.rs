use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(connect_status))
        .route("/enable", post(enable_connect))
        .route("/disable", post(disable_connect))
        .route("/devices", get(list_connect_devices))
        .route("/transfer", post(transfer_playback))
        .route("/lecture-experimentale", post(regler_lecture_experimentale))
}

/// Le statut du récepteur, plus l'état du réglage « Lecture Spotify
/// (expérimental) » (#6018), que le récepteur ne connaît pas.
async fn statut_complet(state: &AppState) -> Value {
    let mut statut = state.spotify_connect.status().await;
    if let Some(objet) = statut.as_object_mut() {
        objet.insert(
            "lecture_experimentale".into(),
            Value::Bool(tune_core::streaming::spotify_lecture::lecture_activee(
                &state.backend,
            )),
        );
    }
    statut
}

#[derive(Deserialize)]
struct CorpsLectureExperimentale {
    enabled: bool,
}

/// #6018 — active ou désactive la lecture Spotify par librespot. Désactivée
/// par défaut : librespot n'est pas un client officiel de Spotify.
async fn regler_lecture_experimentale(
    State(state): State<AppState>,
    Json(corps): Json<CorpsLectureExperimentale>,
) -> impl IntoResponse {
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = reglages.set(
        tune_core::streaming::spotify_lecture::CLE_REGLAGE,
        if corps.enabled { "true" } else { "false" },
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("réglage non enregistré : {e}")})),
        )
            .into_response();
    }
    info!(enabled = corps.enabled, "spotify_lecture_experimentale");
    Json(statut_complet(&state).await).into_response()
}

async fn spotify_token(state: &AppState) -> Option<String> {
    let registry = state.services.lock().await;
    let svc = registry.get("spotify")?;
    drop(registry);
    let svc = svc.read().await;
    let tokens = svc.save_tokens()?;
    tokens.get("access_token")?.as_str().map(Into::into)
}

async fn connect_status(State(state): State<AppState>) -> Json<Value> {
    Json(statut_complet(&state).await)
}

#[derive(Deserialize)]
struct EnableBody {
    zone_id: Option<i64>,
    device_name: Option<String>,
}

async fn enable_connect(
    State(state): State<AppState>,
    Json(body): Json<EnableBody>,
) -> Json<Value> {
    let zone_id = body.zone_id.unwrap_or(1);

    if let Some(ref name) = body.device_name {
        info!(name, "spotify_connect_custom_name");
    }

    if let Err(e) = state.spotify_connect.enable(zone_id).await {
        let mut status = statut_complet(&state).await;
        if let Some(object) = status.as_object_mut() {
            object.insert("error".into(), Value::String(e));
        }
        return Json(status);
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("spotify_connect_enabled", "true").ok();
    settings
        .set("spotify_connect_zone_id", &zone_id.to_string())
        .ok();

    Json(statut_complet(&state).await)
}

async fn disable_connect(State(state): State<AppState>) -> Json<Value> {
    state.spotify_connect.disable().await;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("spotify_connect_enabled", "false").ok();

    Json(statut_complet(&state).await)
}

async fn list_connect_devices(State(state): State<AppState>) -> impl IntoResponse {
    let Some(token) = spotify_token(&state).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Spotify not authenticated"})),
        )
            .into_response();
    };
    let resp = state
        .http_client
        .get("https://api.spotify.com/v1/me/player/devices")
        .bearer_auth(&token)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let body: Value = r.json().await.unwrap_or(json!({"devices": []}));
            Json(body.get("devices").cloned().unwrap_or(json!([]))).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Spotify API error: {e}")})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct TransferBody {
    device_id: String,
    play: Option<bool>,
}

async fn transfer_playback(
    State(state): State<AppState>,
    Json(body): Json<TransferBody>,
) -> impl IntoResponse {
    let Some(token) = spotify_token(&state).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Spotify not authenticated"})),
        )
            .into_response();
    };
    let payload = json!({
        "device_ids": [body.device_id],
        "play": body.play.unwrap_or(true),
    });
    let resp = state
        .http_client
        .put("https://api.spotify.com/v1/me/player")
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            if status == 204 || status == 200 {
                Json(json!({"status": "transferred", "device_id": body.device_id})).into_response()
            } else {
                let err: Value = r.json().await.unwrap_or(json!({"error": "unknown"}));
                (StatusCode::BAD_GATEWAY, Json(err)).into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("Spotify API error: {e}")})),
        )
            .into_response(),
    }
}

pub async fn auto_start(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get("spotify_connect_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let zone_id: i64 = settings
        .get("spotify_connect_zone_id")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    if !tune_core::streaming::spotify_connect::binary_available() {
        info!("spotify_connect_auto_start_skipped: librespot not found");
        return;
    }

    match state.spotify_connect.enable(zone_id).await {
        Ok(()) => info!(zone_id, "spotify_connect_auto_started"),
        Err(e) => info!(error = %e, "spotify_connect_auto_start_failed"),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::state::AppState;

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
            .body(
                corps
                    .map(|c| Body::from(c.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let rep = app.clone().oneshot(req).await.unwrap();
        let statut = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    /// #6018 — « Lecture Spotify (expérimental) » : désactivée par défaut,
    /// annoncée dans le statut, persistée par sa route.
    #[tokio::test]
    async fn spotify_6018_le_reglage_experimental_se_lit_et_s_ecrit() {
        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let db = state.backend.clone();
        let app = crate::routes::router_with_plugins(state, Vec::new());
        let (statut, corps) = appel(&app, "GET", "/api/v1/spotify-connect/status", None).await;
        assert_eq!(statut, StatusCode::OK);
        assert_eq!(
            corps["lecture_experimentale"], false,
            "désactivée par défaut : {corps}"
        );

        let (statut, corps) = appel(
            &app,
            "POST",
            "/api/v1/spotify-connect/lecture-experimentale",
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["lecture_experimentale"], true);
        assert!(
            tune_core::streaming::spotify_lecture::lecture_activee(&db),
            "persistée"
        );

        let (_, corps) = appel(
            &app,
            "POST",
            "/api/v1/spotify-connect/lecture-experimentale",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        assert_eq!(corps["lecture_experimentale"], false);
        assert!(!tune_core::streaming::spotify_lecture::lecture_activee(&db));
    }
}
