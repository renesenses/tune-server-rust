use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

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
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare(
            "SELECT r.album_id, a.title, ar.name, r.rating, r.note, r.created_at \
             FROM album_ratings r \
             LEFT JOIN albums a ON r.album_id = a.id \
             LEFT JOIN artists ar ON a.artist_id = ar.id \
             WHERE r.profile_id = ? \
             ORDER BY r.rating DESC",
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![profile_id], |row| {
                Ok(json!({
                    "album_id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "rating": row.get::<_, Option<i32>>(3).ok().flatten(),
                    "note": row.get::<_, Option<String>>(4).ok().flatten(),
                    "created_at": row.get::<_, Option<String>>(5).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
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
