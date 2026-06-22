use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::rating_repo::RatingRepo;

#[derive(Deserialize)]
pub(super) struct ExportRatingQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct ImportRatingItem {
    album_id: i64,
    rating: i32,
    note: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImportRatingsBody {
    profile_id: Option<i64>,
    ratings: Vec<ImportRatingItem>,
}

pub(super) async fn export_ratings(
    State(state): State<AppState>,
    Query(q): Query<ExportRatingQuery>,
) -> Result<Json<Value>, AppError> {
    let profile_id = q.profile_id.unwrap_or(1);
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    let sql = format!(
        "SELECT r.album_id, a.title, ar.name, r.rating, r.note, r.created_at \
         FROM album_ratings r \
         LEFT JOIN albums a ON r.album_id = a.id \
         LEFT JOIN artists ar ON a.artist_id = ar.id \
         WHERE r.profile_id = {p1} \
         ORDER BY r.rating DESC"
    );
    let rows = state
        .backend
        .query_many(&sql, &[&profile_id as &dyn ToSqlValue])
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "album_id": r.get(0).and_then(|v| v.as_i64()),
                "album_title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "rating": r.get(3).and_then(|v| v.as_i64()),
                "note": r.get(4).and_then(|v| v.as_string()),
                "created_at": r.get(5).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!({
        "profile_id": profile_id,
        "ratings": items,
        "count": items.len(),
    })))
}

pub(super) async fn import_ratings(
    State(state): State<AppState>,
    Json(body): Json<ImportRatingsBody>,
) -> Json<Value> {
    let profile_id = body.profile_id.unwrap_or(1);
    let repo = RatingRepo::with_backend(state.backend.clone());
    let mut imported = 0i32;
    let mut failed = 0i32;
    for item in &body.ratings {
        match repo.rate_album(item.album_id, profile_id, item.rating, item.note.as_deref()) {
            Ok(_) => imported += 1,
            Err(_) => failed += 1,
        }
    }
    Json(json!({
        "imported": imported,
        "failed": failed,
        "total": body.ratings.len(),
    }))
}
