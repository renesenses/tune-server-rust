use axum::{
    Json, Router,
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_skins))
        .route("/active", get(get_active_skin).post(set_active_skin))
        .route("/install", post(install_skin))
        .route("/{skin_id}", delete(uninstall_skin))
        .route("/{skin_id}", get(get_skin_detail))
        .route("/{skin_id}/preview", get(get_skin_preview))
}

async fn list_skins(State(state): State<AppState>) -> Json<serde_json::Value> {
    let skins = state.skin_manager.list();
    let active = get_active_skin_id(&state);

    let items: Vec<_> = skins
        .iter()
        .map(|s| {
            json!({
                "id": s.manifest.id,
                "name": s.manifest.name,
                "version": s.manifest.version,
                "author": s.manifest.author,
                "description": s.manifest.description,
                "framework": s.manifest.framework,
                "api_version": s.manifest.api_version,
                "premium": s.manifest.premium,
                "tags": s.manifest.tags,
                "has_preview": s.manifest.preview.is_some(),
                "size_bytes": s.size_bytes,
                "active": s.manifest.id == active,
                "url": format!("/{}", if s.manifest.id == "default" { "" } else { &s.manifest.id }),
            })
        })
        .collect();

    Json(json!({
        "skins": items,
        "active": active,
        "count": items.len(),
    }))
}

async fn get_skin_detail(
    State(state): State<AppState>,
    Path(skin_id): Path<String>,
) -> impl IntoResponse {
    match state.skin_manager.get(&skin_id) {
        Some(skin) => {
            let active = get_active_skin_id(&state);
            Json(json!({
                "id": skin.manifest.id,
                "name": skin.manifest.name,
                "version": skin.manifest.version,
                "author": skin.manifest.author,
                "description": skin.manifest.description,
                "framework": skin.manifest.framework,
                "api_version": skin.manifest.api_version,
                "premium": skin.manifest.premium,
                "tags": skin.manifest.tags,
                "has_preview": skin.manifest.preview.is_some(),
                "size_bytes": skin.size_bytes,
                "active": skin.manifest.id == active,
                "entry": skin.manifest.entry,
                "min_server_version": skin.manifest.min_server_version,
            }))
            .into_response()
        }
        None => (StatusCode::NOT_FOUND, "skin not found").into_response(),
    }
}

async fn get_active_skin(State(state): State<AppState>) -> Json<serde_json::Value> {
    let active_id = get_active_skin_id(&state);
    let skin = state.skin_manager.get(&active_id);

    Json(json!({
        "id": active_id,
        "name": skin.as_ref().map(|s| s.manifest.name.as_str()).unwrap_or("Tune"),
        "version": skin.as_ref().map(|s| s.manifest.version.as_str()).unwrap_or("1.0.0"),
    }))
}

#[derive(Deserialize)]
struct SetActiveSkinRequest {
    skin_id: String,
}

async fn set_active_skin(
    State(state): State<AppState>,
    Json(body): Json<SetActiveSkinRequest>,
) -> impl IntoResponse {
    if state.skin_manager.get(&body.skin_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "skin not found"})),
        )
            .into_response();
    }

    use tune_core::db::backend::ToSqlValue;
    let engine = state.backend.engine();
    let p1 = if engine == tune_core::db::engine::Engine::Postgres {
        "$1"
    } else {
        "?"
    };
    let p2 = if engine == tune_core::db::engine::Engine::Postgres {
        "$2"
    } else {
        "?"
    };

    let _ = state.backend.execute(
        &format!(
            "INSERT INTO settings (key, value) VALUES ({p1}, {p2}) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        ),
        &[
            &"active_skin" as &dyn ToSqlValue,
            &body.skin_id as &dyn ToSqlValue,
        ],
    );

    tracing::info!(skin_id = %body.skin_id, "active_skin_changed");

    if let Some(ref bus) = Some(&state.event_bus) {
        bus.emit("skin.changed", json!({ "skin_id": body.skin_id }));
    }

    Json(json!({
        "ok": true,
        "active": body.skin_id,
        "message": "Restart the server to apply the new skin as default"
    }))
    .into_response()
}

async fn install_skin(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut zip_data: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("skin") || field.content_type() == Some("application/zip") {
            match field.bytes().await {
                Ok(data) => {
                    zip_data = Some(data.to_vec());
                    break;
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("read upload: {e}")})),
                    )
                        .into_response();
                }
            }
        }
    }

    let zip_data = match zip_data {
        Some(d) => d,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no skin zip file in upload"})),
            )
                .into_response();
        }
    };

    match state.skin_manager.install_from_zip(&zip_data) {
        Ok(manifest) => Json(json!({
            "ok": true,
            "installed": {
                "id": manifest.id,
                "name": manifest.name,
                "version": manifest.version,
                "author": manifest.author,
            }
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

async fn uninstall_skin(
    State(state): State<AppState>,
    Path(skin_id): Path<String>,
) -> impl IntoResponse {
    match state.skin_manager.uninstall(&skin_id) {
        Ok(()) => Json(json!({"ok": true, "removed": skin_id})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

async fn get_skin_preview(
    State(state): State<AppState>,
    Path(skin_id): Path<String>,
) -> impl IntoResponse {
    let skin = match state.skin_manager.get(&skin_id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "skin not found").into_response(),
    };

    let preview_file = match &skin.manifest.preview {
        Some(p) => p.clone(),
        None => return (StatusCode::NOT_FOUND, "no preview").into_response(),
    };

    let preview_path = skin.path.join(&preview_file);
    match tokio::fs::read(&preview_path).await {
        Ok(data) => {
            let mime = if preview_file.ends_with(".png") {
                "image/png"
            } else if preview_file.ends_with(".webp") {
                "image/webp"
            } else {
                "image/jpeg"
            };
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(mime).unwrap(),
            );
            headers.insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=3600"),
            );
            (headers, data).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "preview file not found").into_response(),
    }
}

fn get_active_skin_id(state: &AppState) -> String {
    use tune_core::db::backend::ToSqlValue;
    let engine = state.backend.engine();
    let p1 = if engine == tune_core::db::engine::Engine::Postgres {
        "$1"
    } else {
        "?"
    };
    state
        .backend
        .query_one(
            &format!("SELECT value FROM settings WHERE key = {p1}"),
            &[&"active_skin" as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_string()))
        .unwrap_or_else(|| "default".into())
}
