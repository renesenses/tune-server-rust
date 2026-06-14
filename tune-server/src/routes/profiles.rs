use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateProfile {
    #[serde(alias = "username")]
    name: String,
    #[serde(alias = "display_name")]
    avatar_color: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProfile {
    #[serde(alias = "display_name")]
    name: Option<String>,
    #[serde(alias = "avatar_path")]
    avatar_color: Option<String>,
}

#[derive(Deserialize)]
struct FavoriteAction {
    item_type: String,
    item_id: i64,
}

#[derive(Deserialize)]
struct FavoritesQuery {
    item_type: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct SwitchProfile {
    profile_id: i64,
    pin: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct CheckFavoritesBody {
    item_type: String,
    item_ids: Vec<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_profiles).post(create_profile))
        .route("/active", get(get_active_profile))
        .route("/current", get(get_active_profile))
        .route("/switch", post(switch_profile))
        .route("/deactivate", post(deactivate_profile))
        .route("/search", get(search_profiles))
        .route(
            "/{id}",
            get(get_profile).put(update_profile).delete(delete_profile),
        )
        .route("/{id}/activate", post(activate_profile))
        .route("/{id}/favorites", get(list_favorites))
        .route("/{id}/favorites/add", post(add_favorite))
        .route("/{id}/favorites/remove", post(remove_favorite))
        .route(
            "/{id}/settings",
            get(profile_settings).post(update_profile_settings),
        )
        .route("/{id}/stats", get(profile_stats))
        .route("/{id}/history", get(profile_history))
        .route("/{id}/favorites/check", post(check_favorites))
}

async fn list_profiles(State(state): State<AppState>) -> Json<Value> {
    let repo = ProfileRepo::new(state.db);
    let items = repo.list().unwrap_or_default();
    Json(json!(items))
}

async fn get_active_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let profile_id: i64 = settings
        .get("active_profile_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let repo = ProfileRepo::new(state.db);
    let profile = repo.get(profile_id).ok().flatten();
    Json(json!({
        "active_profile_id": profile_id,
        "profile": profile,
    }))
}

async fn switch_profile(
    State(state): State<AppState>,
    Json(body): Json<SwitchProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db.clone());
    match repo.get(body.profile_id) {
        Ok(Some(profile)) => {
            let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
            settings
                .set("active_profile_id", &body.profile_id.to_string())
                .ok();
            Json(json!({
                "active_profile_id": body.profile_id,
                "profile": profile,
            }))
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "profile not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn deactivate_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    settings.set("active_profile_id", "1").ok();
    Json(json!({ "active_profile_id": serde_json::Value::Null }))
}

async fn activate_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db.clone());
    match repo.get(id) {
        Ok(Some(profile)) => {
            let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
            settings.set("active_profile_id", &id.to_string()).ok();
            Json(json!({
                "active_profile_id": id,
                "profile": profile,
            }))
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "profile not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(p)) => Json(json!(p)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_profile(
    State(state): State<AppState>,
    Json(body): Json<CreateProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.create(&body.name, None, body.avatar_color.as_deref()) {
        Ok(id) => {
            // Return the full profile object so the web client can use it directly
            let profile = repo.get(id).ok().flatten();
            let value = profile
                .map(|p| json!(p))
                .unwrap_or_else(|| json!({"id": id, "name": body.name}));
            (StatusCode::CREATED, Json(value)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.update(id, body.name.as_deref(), body.avatar_color.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn list_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<FavoritesQuery>,
) -> Json<Value> {
    let repo = ProfileRepo::new(state.db);
    let items = repo
        .list_favorites(id, q.item_type.as_deref())
        .unwrap_or_default();
    Json(json!(items))
}

async fn add_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<FavoriteAction>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.add_favorite(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<FavoriteAction>,
) -> impl IntoResponse {
    let repo = ProfileRepo::new(state.db);
    match repo.remove_favorite(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// --- Advanced profile routes ---

async fn profile_settings(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = format!("profile_{id}_settings");
    let value = settings
        .get(&key)
        .ok()
        .flatten()
        .unwrap_or_else(|| "{}".to_string());
    let parsed: Value = serde_json::from_str(&value).unwrap_or(json!({}));
    Json(parsed)
}

async fn update_profile_settings(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let key = format!("profile_{id}_settings");
    let serialized = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    match settings.set(&key, &serialized) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn profile_stats(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let result: Result<Vec<(String, i64)>, String> = (|| {
        let conn = state.db.connection().lock().map_err(|e| format!("{e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT item_type, COUNT(*) FROM favorites WHERE profile_id = ? GROUP BY item_type",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![id], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, i64>(1).unwrap_or(0),
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    })();

    match result {
        Ok(rows) => {
            let mut stats = json!({});
            for (item_type, count) in &rows {
                stats[item_type] = json!(count);
            }
            Json(json!({
                "profile_id": id,
                "favorites_by_type": stats,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn profile_history(
    State(state): State<AppState>,
    Path(_id): Path<i64>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let repo = HistoryRepo::new(state.db);
    let limit = q.limit.unwrap_or(50);
    let items = repo.recent(limit).unwrap_or_default();
    Json(json!(items))
}

async fn search_profiles(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let repo = ProfileRepo::new(state.db);
    let all = repo.list().unwrap_or_default();
    let query = q.q.unwrap_or_default().to_lowercase();
    if query.is_empty() {
        return Json(json!(all));
    }
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|p| p.name.to_lowercase().contains(&query))
        .collect();
    Json(json!(filtered))
}

async fn check_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<CheckFavoritesBody>,
) -> Json<Value> {
    let repo = ProfileRepo::new(state.db);
    let results: Vec<Value> = body
        .item_ids
        .iter()
        .map(|&item_id| {
            let is_fav = repo
                .is_favorite(id, &body.item_type, item_id)
                .unwrap_or(false);
            json!({ "item_id": item_id, "is_favorite": is_fav })
        })
        .collect();
    Json(json!(results))
}
