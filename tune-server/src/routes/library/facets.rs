use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct FacetQuery {
    /// Comma-separated facet fields to compute (default: the common set).
    fields: Option<String>,
    /// Max values per facet (default 200).
    limit: Option<i64>,
}

/// GET /api/v1/library/facets?fields=genre,label,year,artist,country,mood,source
///
/// Returns `{ "<field>": [{ "value": string, "count": number }] }` for each
/// requested facet — full-library counts, unlike the client's loaded-window
/// aggregation. `country`/`mood`/`source` are read from the open `track_metadata`
/// key/value store (release_country / mood / source_media), which the client
/// cannot aggregate without a per-track fetch.
pub(super) async fn library_facets(
    Query(q): Query<FacetQuery>,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let requested: Vec<String> = q
        .fields
        .as_deref()
        .unwrap_or("genre,label,year,artist,country,mood,source")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut out = serde_json::Map::new();
    for field in requested {
        // The column / key is chosen from this fixed allow-list only, so the
        // formatted SQL below is never influenced by request input.
        let rows: Vec<(String, i64)> = match field.as_str() {
            "genre" => column_facet(&state, "genre", limit),
            "label" => column_facet(&state, "label", limit),
            "year" => column_facet(&state, "year", limit),
            "artist" => column_facet(&state, "artist_name", limit),
            "country" => kv_facet(&state, "release_country", limit),
            "mood" => kv_facet(&state, "mood", limit),
            "source" => kv_facet(&state, "source_media", limit),
            _ => continue,
        };
        let arr: Vec<Value> = rows
            .into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect();
        out.insert(field, Value::Array(arr));
    }
    Ok(Json(Value::Object(out)))
}

/// Count distinct values of a fixed `tracks` column.
fn column_facet(state: &AppState, col: &str, limit: i64) -> Vec<(String, i64)> {
    let sql = format!(
        "SELECT {col}, COUNT(*) AS n FROM tracks \
         WHERE {col} IS NOT NULL AND CAST({col} AS TEXT) <> '' \
         GROUP BY {col} ORDER BY n DESC LIMIT {limit}"
    );
    state
        .backend
        .query_many(&sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let v = it.next()?;
            let c = it.next()?;
            let value = v
                .as_string()
                .or_else(|| v.as_i64().map(|n| n.to_string()))?;
            Some((value, c.as_i64().unwrap_or(0)))
        })
        .collect()
}

/// Count distinct values of an extended tag in the `track_metadata` k/v store.
fn kv_facet(state: &AppState, key: &str, limit: i64) -> Vec<(String, i64)> {
    let sql = format!(
        "SELECT value, COUNT(DISTINCT track_id) AS n FROM track_metadata \
         WHERE key = '{key}' AND value <> '' \
         GROUP BY value ORDER BY n DESC LIMIT {limit}"
    );
    state
        .backend
        .query_many(&sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let value = it.next()?.as_string()?;
            let count = it.next()?.as_i64().unwrap_or(0);
            Some((value, count))
        })
        .collect()
}
