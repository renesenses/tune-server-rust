use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::SqlValue;
use tune_core::db::engine::Engine;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct FacetQuery {
    /// Comma-separated facet fields to compute (default: the common set).
    fields: Option<String>,
    /// Max values per facet (default 200).
    limit: Option<i64>,
    // Optional active filters — when present, each facet is counted over the
    // narrowed track set (cumulative faceting, Dominique: "le genre filtre les
    // labels"). Same names as /library/tracks. A facet never filters on its own
    // field, so its other values stay visible to switch to.
    genre: Option<String>,
    year: Option<i32>,
    format: Option<String>,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
    source: Option<String>,
    label: Option<String>,
    composer: Option<String>,
    artist: Option<String>,
    country: Option<String>,
    mood: Option<String>,
    source_media: Option<String>,
    q: Option<String>,
}

/// Build the WHERE conditions (over alias `t` = tracks) for the active filters,
/// skipping the facet's own field so its alternatives remain countable. Values
/// are always bound parameters; column/key names come from fixed literals only.
fn build_conditions(q: &FacetQuery, engine: Engine, exclude: &str) -> (Vec<String>, Vec<SqlValue>) {
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    let mut idx = 1usize;
    let ph = |i: usize| match engine {
        Engine::Sqlite => "?".to_string(),
        Engine::Postgres => format!("${i}"),
    };

    if exclude != "genre" {
        if let Some(g) = q.genre.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "(LOWER(t.genre) = LOWER({p}) OR t.genres LIKE {p2})",
                p = ph(idx),
                p2 = ph(idx + 1)
            ));
            params.push(SqlValue::Text(g.to_string()));
            params.push(SqlValue::Text(format!("%\"{g}\"%")));
            idx += 2;
        }
    }
    if exclude != "year" {
        if let Some(y) = q.year {
            conds.push(format!("t.year = {}", ph(idx)));
            params.push(SqlValue::Int(y as i64));
            idx += 1;
        }
    }
    if let Some(f) = q.format.as_deref().filter(|s| !s.is_empty()) {
        conds.push(format!("LOWER(t.format) = LOWER({})", ph(idx)));
        params.push(SqlValue::Text(f.to_string()));
        idx += 1;
    }
    if let Some(sr) = q.sample_rate {
        conds.push(format!("t.sample_rate = {}", ph(idx)));
        params.push(SqlValue::Int(sr as i64));
        idx += 1;
    }
    if let Some(bd) = q.bit_depth {
        conds.push(format!("t.bit_depth = {}", ph(idx)));
        params.push(SqlValue::Int(bd as i64));
        idx += 1;
    }
    if let Some(s) = q.source.as_deref().filter(|s| !s.is_empty()) {
        conds.push(format!("t.source = {}", ph(idx)));
        params.push(SqlValue::Text(s.to_string()));
        idx += 1;
    }
    if exclude != "label" {
        if let Some(l) = q.label.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!("LOWER(t.label) LIKE LOWER({})", ph(idx)));
            params.push(SqlValue::Text(format!("%{l}%")));
            idx += 1;
        }
    }
    if let Some(c) = q.composer.as_deref().filter(|s| !s.is_empty()) {
        conds.push(format!("LOWER(t.composer) LIKE LOWER({})", ph(idx)));
        params.push(SqlValue::Text(format!("%{c}%")));
        idx += 1;
    }
    if exclude != "artist" {
        if let Some(a) = q.artist.as_deref().filter(|s| !s.is_empty()) {
            // `tracks` has no artist_name column (artist is a FK to `artists`),
            // and these conditions run against `FROM tracks t` with no join, so
            // resolve the name via a subquery rather than the phantom
            // t.artist_name (forum #1189).
            conds.push(format!(
                "t.artist_id IN (SELECT id FROM artists WHERE name = {})",
                ph(idx)
            ));
            params.push(SqlValue::Text(a.to_string()));
            idx += 1;
        }
    }
    // Extended-tag filters via the open `track_metadata` k/v store. `source`
    // facet == source_media key. Key is a fixed literal; value is bound.
    for (opt, key, own) in [
        (&q.country, "release_country", "country"),
        (&q.mood, "mood", "mood"),
        (&q.source_media, "source_media", "source"),
    ] {
        if exclude == own {
            continue;
        }
        if let Some(v) = opt.as_deref().filter(|s| !s.is_empty()) {
            conds.push(format!(
                "EXISTS (SELECT 1 FROM track_metadata tm \
                 WHERE tm.track_id = t.id AND tm.key = '{key}' AND tm.value = {})",
                ph(idx)
            ));
            params.push(SqlValue::Text(v.to_string()));
            idx += 1;
        }
    }
    if let Some(query) = q.q.as_deref().filter(|s| !s.is_empty()) {
        // Artist match via subquery — no artist_name column / no join here (#1189).
        conds.push(format!(
            "(LOWER(t.title) LIKE LOWER({p}) OR t.artist_id IN \
             (SELECT id FROM artists WHERE LOWER(name) LIKE LOWER({p2})))",
            p = ph(idx),
            p2 = ph(idx + 1)
        ));
        let like = format!("%{query}%");
        params.push(SqlValue::Text(like.clone()));
        params.push(SqlValue::Text(like));
    }
    (conds, params)
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
    // `limit <= 0` means "no limit" (show every facet value); otherwise clamp to
    // a sane ceiling. Absent → the historical default of 200.
    let limit: Option<i64> = match q.limit {
        Some(n) if n <= 0 => None,
        Some(n) => Some(n.clamp(1, 5000)),
        None => Some(200),
    };
    let requested: Vec<String> = q
        .fields
        .as_deref()
        .unwrap_or("genre,label,year,artist,country,mood,source")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let engine = state.backend.engine();
    let mut out = serde_json::Map::new();
    for field in requested {
        // Conditions narrow the count by the OTHER active facets (cumulative).
        let (conds, params) = build_conditions(&q, engine, &field);
        // The column / key is chosen from this fixed allow-list only, so the
        // formatted SQL below is never influenced by request input.
        let rows: Vec<(String, i64)> = match field.as_str() {
            "genre" => column_facet(&state, "genre", limit, &conds, &params),
            "label" => column_facet(&state, "label", limit, &conds, &params),
            "year" => column_facet(&state, "year", limit, &conds, &params),
            "artist" => artist_facet(&state, limit, &conds, &params),
            "country" => kv_facet(&state, "release_country", limit, &conds, &params),
            "mood" => kv_facet(&state, "mood", limit, &conds, &params),
            "source" => kv_facet(&state, "source_media", limit, &conds, &params),
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

/// Count distinct values of a fixed `tracks` column, optionally narrowed by the
/// active-facet conditions (over alias `t`).
fn column_facet(
    state: &AppState,
    col: &str,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let extra = if conds.is_empty() {
        String::new()
    } else {
        format!(" AND {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT t.{col}, COUNT(*) AS n FROM tracks t \
         WHERE t.{col} IS NOT NULL AND CAST(t.{col} AS TEXT) <> ''{extra} \
         GROUP BY t.{col} ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
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

/// Count tracks per artist. Unlike other facets, the artist name is NOT a column
/// on `tracks` (it stores only `artist_id`); it lives on the joined `artists`
/// table. The old code queried the phantom `t.artist_name` → SQL error → empty
/// facet (forum #1189). Join `artists` and group on its name. The `conds` are
/// self-contained (they reference `t.*` or subqueries), so the join is additive.
fn artist_facet(
    state: &AppState,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let extra = if conds.is_empty() {
        String::new()
    } else {
        format!(" AND {}", conds.join(" AND "))
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT ar.name, COUNT(*) AS n FROM tracks t \
         JOIN artists ar ON t.artist_id = ar.id \
         WHERE ar.name IS NOT NULL AND ar.name <> ''{extra} \
         GROUP BY ar.name ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let v = it.next()?;
            let c = it.next()?;
            let value = v.as_string()?;
            Some((value, c.as_i64().unwrap_or(0)))
        })
        .collect()
}

/// Count distinct values of an extended tag in the `track_metadata` k/v store,
/// optionally narrowed to the tracks matching the active-facet conditions.
fn kv_facet(
    state: &AppState,
    key: &str,
    limit: Option<i64>,
    conds: &[String],
    params: &[SqlValue],
) -> Vec<(String, i64)> {
    let narrow = if conds.is_empty() {
        String::new()
    } else {
        format!(
            " AND track_id IN (SELECT t.id FROM tracks t WHERE {})",
            conds.join(" AND ")
        )
    };
    let limit_clause = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT value, COUNT(DISTINCT track_id) AS n FROM track_metadata \
         WHERE key = '{key}' AND value <> ''{narrow} \
         GROUP BY value ORDER BY n DESC{limit_clause}"
    );
    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();
    state
        .backend
        .query_many(&sql, &bound)
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
