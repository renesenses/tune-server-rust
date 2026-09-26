use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(sacd_status))
        .route("/iso-status", get(sacd_iso_status))
        .route("/disc", get(sacd_disc_info))
        .route("/rip", post(start_sacd_rip))
        .route("/rip/status", get(sacd_rip_status))
}

// The diagnostic never queues an unbounded series of external processes.
// A busy or timed-out probe is unknown, not "tool missing".
static ISO_PROBE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

async fn sacd_iso_status() -> Result<Json<Value>, AppError> {
    iso_status_with_probe(tune_core::audio::iso_sacd::sacd_extract_available()).await
}

async fn iso_status_with_probe(
    probe: impl std::future::Future<Output = Result<bool, String>>,
) -> Result<Json<Value>, AppError> {
    let _permit = ISO_PROBE
        .try_acquire()
        .map_err(|_| AppError::service_unavailable("SACD ISO extractor probe already running"))?;
    let available = probe.await.map_err(AppError::service_unavailable)?;
    Ok(Json(json!({
        "available": available,
        "tool": if available { "sacd_extract" } else { "none" },
    })))
}

/// Check if sacd_extract or similar tool is available.
async fn sacd_status() -> Json<Value> {
    let sacd_extract = tokio::process::Command::new("which")
        .arg("sacd_extract")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    Json(json!({
        "available": sacd_extract,
        "tool": if sacd_extract { "sacd_extract" } else { "none" },
        "message": if sacd_extract {
            "sacd_extract found"
        } else {
            "SACD ripping requires sacd_extract and specialized hardware (compatible Blu-ray drive with SACD support)"
        },
        "requirements": [
            "Compatible Blu-ray/SACD drive",
            "sacd_extract binary installed",
            "Physical SACD disc inserted",
        ],
    }))
}

/// Read SACD disc information (stub — requires hardware).
async fn sacd_disc_info() -> Json<Value> {
    Json(json!({
        "disc_detected": false,
        "title": null,
        "artist": null,
        "tracks_stereo": [],
        "tracks_multichannel": [],
        "layers": {
            "stereo": false,
            "multichannel": false,
            "cd_layer": false,
        },
        "message": "SACD disc info requires compatible hardware. Insert a disc and ensure sacd_extract is available.",
    }))
}

#[derive(Deserialize)]
struct SacdRipRequest {
    /// Output directory
    output_dir: Option<String>,
    /// Output format: "dsf" (DSD), "dff" (DSD), "iso"
    format: Option<String>,
    /// Layer to rip: "stereo", "multichannel", or "both"
    layer: Option<String>,
}

/// Start an SACD rip (stub — requires hardware).
async fn start_sacd_rip(
    State(state): State<AppState>,
    Json(body): Json<SacdRipRequest>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    let output_dir = body
        .output_dir
        .or_else(|| settings.get("sacd_rip_output_dir").ok().flatten())
        .unwrap_or_else(|| {
            // #4770 : un dossier par compte, jamais un nom fixe partagé.
            tune_core::chemins_de_travail::racine_de_travail("tune-sacd-rip")
                .to_string_lossy()
                .to_string()
        });
    let format = body.format.unwrap_or_else(|| "dsf".into());
    let layer = body.layer.unwrap_or_else(|| "stereo".into());

    let rip_id = uuid::Uuid::new_v4().to_string();

    let rip_state = json!({
        "id": rip_id,
        "status": "not_available",
        "output_dir": output_dir,
        "format": format,
        "layer": layer,
        "progress": 0,
        "message": "SACD ripping requires compatible hardware. This is a stub implementation.",
    });

    settings
        .set("sacd_rip_current", &serde_json::to_string(&rip_state)?)
        .ok();

    Ok(Json(rip_state))
}

/// Get current SACD rip status.
async fn sacd_rip_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let current = settings
        .get("sacd_rip_current")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    match current {
        Some(rip) => Json(rip),
        None => Json(json!({
            "status": "idle",
            "message": "No SACD rip in progress",
        })),
    }
}

#[cfg(test)]
mod iso_status_tests_3234 {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn iso_status_http_contract_and_busy_probe_remains_unknown() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let app = router().with_state(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/iso-status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let available = json["available"].as_bool().expect("explicit availability");
        assert_eq!(
            json["tool"],
            if available { "sacd_extract" } else { "none" }
        );
        assert!(
            json.get("requirements").is_none(),
            "ISO is not a physical drive"
        );

        let permit = ISO_PROBE.acquire().await.unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/iso-status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert!(
            json.get("available").is_none(),
            "a concurrent request must not claim absence"
        );
        drop(permit);

        // Exercise the same HTTP handler's failure conversion, without
        // changing PATH or requiring a globally installed fake tool.
        use axum::response::IntoResponse;
        let timeout_response =
            iso_status_with_probe(async { Err("SACD ISO extractor probe timed out".into()) })
                .await
                .into_response();
        assert_eq!(timeout_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: Value =
            serde_json::from_slice(&to_bytes(timeout_response.into_body(), 4096).await.unwrap())
                .unwrap();
        assert!(body.get("available").is_none());
        assert_eq!(
            ISO_PROBE.available_permits(),
            1,
            "failure releases the permit"
        );
        let next = app
            .oneshot(
                Request::builder()
                    .uri("/iso-status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(next.status(), StatusCode::OK);
    }
}
