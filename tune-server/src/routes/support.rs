//! Proxy HTTP vers l'API support premium de mozaiklabs.
//!
//! Le token OAuth premium (`mozaik_access_token`) vit en settings côté serveur ;
//! le client web ne l'a jamais → tout passe par ici. Voir
//! [`tune_core::cloud::support`]. Le gate premium autoritatif est côté
//! mozaiklabs (`auth.premium`) : un 403 y est renvoyé tel quel au client.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::cloud::support;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tickets", get(list).post(create))
        .route("/tickets/{id}", get(detail))
        .route("/tickets/{id}/reply", post(reply))
}

#[derive(Deserialize)]
struct CreateBody {
    subject: String,
    body: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
}

async fn list(State(state): State<AppState>) -> Response {
    let token = match token(&state) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    finish(support::list_tickets(&state.http_client, &token).await)
}

async fn create(State(state): State<AppState>, Json(payload): Json<CreateBody>) -> Response {
    let token = match token(&state) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    finish(
        support::create_ticket(
            &state.http_client,
            &token,
            &payload.subject,
            &payload.body,
            payload.category.as_deref(),
        )
        .await,
    )
}

async fn detail(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let token = match token(&state) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    finish(support::get_ticket(&state.http_client, &token, id).await)
}

async fn reply(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<ReplyBody>,
) -> Response {
    let token = match token(&state) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    finish(support::reply(&state.http_client, &token, id, &payload.body).await)
}

/// Lit le token OAuth premium ; 412 si l'utilisateur n'est pas connecté en SSO.
fn token(state: &AppState) -> Result<String, Response> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match settings.get("mozaik_access_token").ok().flatten() {
        Some(t) if !t.is_empty() => Ok(t),
        _ => Err((
            StatusCode::PRECONDITION_FAILED,
            Json(json!({
                "error": "not_connected",
                "message": "Connecte-toi à ton compte Tune (SSO) pour utiliser le support prioritaire.",
            })),
        )
            .into_response()),
    }
}

/// Traduit le `SupportResult` en réponse HTTP, en préservant le status renvoyé
/// par mozaiklabs (401/403/422…).
fn finish(result: support::SupportResult) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err((status, value)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(value),
        )
            .into_response(),
    }
}
