use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

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
    let repo = ProfileRepo::with_backend(state.backend.clone());
    // Don't swallow a DB error as an empty list: the web client would then
    // auto-create "Default" and mask real profiles with no visible error.
    // Log it so a schema/column drift on an older DB is diagnosable.
    let items = repo.list().unwrap_or_else(|e| {
        tracing::error!(error = %e, "list_profiles: repo.list() failed, returning empty");
        Vec::new()
    });
    Json(json!(items))
}

async fn get_active_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let profile_id: i64 = settings
        .get("active_profile_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.get(body.profile_id) {
        Ok(Some(profile)) => {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
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
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("active_profile_id", "1").ok();
    Json(json!({ "active_profile_id": serde_json::Value::Null }))
}

async fn activate_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(profile)) => {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());

    // Free tier: max 1 profile (the default). Premium: unlimited.
    let is_premium = state.license.check_feature(Feature::MultiProfiles).await;
    if !is_premium {
        let count = repo.count().unwrap_or(0);
        if count >= 1 {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "premium_required",
                    "feature": "multi_profiles",
                    "message": "Free tier allows 1 profile. Upgrade to Premium for unlimited profiles.",
                })),
            )
                .into_response();
        }
    }

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
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.update(id, body.name.as_deref(), body.avatar_color.as_deref()) {
        Ok(_) => {
            // Return the updated profile so the client can use it directly
            match repo.get(id) {
                Ok(Some(profile)) => Json(json!(profile)).into_response(),
                Ok(None) => {
                    (StatusCode::NOT_FOUND, "profile not found after update").into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.remove_favorite(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// --- Advanced profile routes ---

async fn profile_settings(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("profile_{id}_settings");
    let serialized = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    match settings.set(&key, &serialized) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn profile_stats(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let b = &state.backend;

    // Favorites by type
    let fav_result: Result<Vec<(String, i64)>, String> = b
        .query_many(
            "SELECT item_type, COUNT(*) FROM favorites WHERE profile_id = ? GROUP BY item_type",
            &[&id as &dyn ToSqlValue],
        )
        .map(|rows| {
            rows.into_iter()
                .map(|r| {
                    (
                        r.get(0).and_then(|v| v.as_string()).unwrap_or_default(),
                        r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                    )
                })
                .collect()
        });

    let mut favorites_by_type = json!({});
    if let Ok(rows) = &fav_result {
        for (item_type, count) in rows {
            favorites_by_type[item_type] = json!(count);
        }
    }

    // Per-profile listening stats (profile_id NULL = legacy entries, belong to default)
    let profile_filter = if id == 1 {
        // Default profile sees entries with profile_id = 1 OR NULL (legacy)
        "(profile_id = 1 OR profile_id IS NULL)".to_string()
    } else {
        format!("profile_id = {id}")
    };

    let listens_row = b
        .query_one(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) \
                 FROM listen_history WHERE {profile_filter}"
            ),
            &[],
        )
        .ok()
        .flatten()
        .unwrap_or_default();

    let total_listens = listens_row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = listens_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);

    let top_artists: Vec<serde_json::Value> = b
        .query_many(
            &format!(
                "SELECT artist_name, COUNT(*) as plays FROM listen_history \
                 WHERE {profile_filter} AND artist_name IS NOT NULL \
                 GROUP BY artist_name ORDER BY plays DESC LIMIT 10"
            ),
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|cols| {
            json!({
                "artist": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let top_tracks: Vec<serde_json::Value> = b
        .query_many(
            &format!(
                "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
                 WHERE {profile_filter} \
                 GROUP BY title, artist_name ORDER BY plays DESC LIMIT 10"
            ),
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|cols| {
            json!({
                "title": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    // Ratings count
    let ratings_count = b
        .query_one(
            "SELECT COUNT(*) FROM album_ratings WHERE profile_id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);

    Json(json!({
        "profile_id": id,
        "favorites_by_type": favorites_by_type,
        "listening": {
            "total_listens": total_listens,
            "total_duration_ms": total_ms,
            "total_hours": (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0,
            "top_artists": top_artists,
            "top_tracks": top_tracks,
        },
        "ratings_count": ratings_count,
    }))
    .into_response()
}

async fn profile_history(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(50);
    let profile_filter = if id == 1 {
        "(profile_id = 1 OR profile_id IS NULL)".to_string()
    } else {
        format!("profile_id = {id}")
    };
    let sql = format!(
        "SELECT id, track_id, title, artist_name, album_title, source, source_id, \
         album_id, duration_ms, listened_at, zone_id \
         FROM listen_history WHERE {profile_filter} \
         ORDER BY listened_at DESC LIMIT ?",
    );
    let rows = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "track_id": cols.get(1).and_then(|v| v.as_i64()),
                "title": cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(3).and_then(|v| v.as_string()),
                "album_title": cols.get(4).and_then(|v| v.as_string()),
                "source": cols.get(5).and_then(|v| v.as_string()).unwrap_or_else(|| "local".into()),
                "source_id": cols.get(6).and_then(|v| v.as_string()),
                "album_id": cols.get(7).and_then(|v| v.as_i64()),
                "duration_ms": cols.get(8).and_then(|v| v.as_i64()).unwrap_or(0),
                "listened_at": cols.get(9).and_then(|v| v.as_string()),
                "zone_id": cols.get(10).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Json(json!(items))
}

async fn search_profiles(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
    let repo = ProfileRepo::with_backend(state.backend.clone());
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
