use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use tune_core::license::Feature;
use tune_core::playlist_transfer::{self, TransferRequest};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/transfer", post(transfer))
        .route("/transfer/preview", post(preview))
}

async fn transfer(
    State(state): State<AppState>,
    Json(req): Json<TransferRequest>,
) -> impl IntoResponse {
    // Premium gate
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::PlaylistTransfer).await
    {
        return resp;
    }

    // Validate services are different
    if req.source_service == req.target_service {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "source and target services must be different"})),
        )
            .into_response();
    }

    match playlist_transfer::transfer_playlist(&state.services, &req).await {
        Ok(report) => Json(json!(report)).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}

async fn preview(
    State(state): State<AppState>,
    Json(req): Json<TransferRequest>,
) -> impl IntoResponse {
    // Premium gate
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::PlaylistTransfer).await
    {
        return resp;
    }

    if req.source_service == req.target_service {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "source and target services must be different"})),
        )
            .into_response();
    }

    match playlist_transfer::preview_transfer(&state.services, &req).await {
        Ok(report) => Json(json!(report)).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}
