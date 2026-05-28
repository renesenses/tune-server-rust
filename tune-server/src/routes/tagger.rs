use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/batch", post(batch_edit_tags))
        .route("/preview", post(preview_batch_edit))
        .route("/album/{id}/auto-number", post(auto_number_album))
        .route("/album/{id}/set-genre", post(set_album_genre))
        .route("/album/{id}/set-year", post(set_album_year))
        .route("/rename-pattern", post(rename_by_pattern))
        .route("/fix-encoding", post(fix_encoding))
        .route("/strip-tags", post(strip_extra_tags))
}

#[derive(Deserialize)]
struct BatchEditRequest {
    track_ids: Vec<i64>,
    fields: BatchFields,
}

#[derive(Deserialize)]
struct BatchFields {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    genre: Option<String>,
    year: Option<String>,
    album_artist: Option<String>,
}

fn get_track_path(state: &AppState, track_id: i64) -> Option<(String, Value)> {
    let conn = state.db.connection().lock().unwrap();
    conn.prepare(
        "SELECT path, title, artist_name, album_title, genre, year FROM tracks WHERE id = ?1",
    )
    .ok()
    .and_then(|mut stmt| {
        stmt.query_row([track_id], |row| {
            let path: String = row.get(0)?;
            let info = json!({
                "id": track_id,
                "path": path.clone(),
                "title": row.get::<_, Option<String>>(1).unwrap_or(None),
                "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                "album_title": row.get::<_, Option<String>>(3).unwrap_or(None),
                "genre": row.get::<_, Option<String>>(4).unwrap_or(None),
                "year": row.get::<_, Option<String>>(5).unwrap_or(None),
            });
            Ok((path, info))
        })
        .ok()
    })
}

fn apply_tags_to_file(path: &str, fields: &BatchFields) -> Result<Vec<String>, String> {
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey, TagExt};

    let tagged = lofty::read_from_path(path).map_err(|e| format!("Failed to read {path}: {e}"))?;
    let mut tagged = tagged;
    let tag = match tagged.primary_tag_mut() {
        Some(t) => t,
        None => return Err(format!("No tag found in {path}")),
    };

    let mut changes = Vec::new();

    if let Some(title) = &fields.title {
        tag.set_title(title.clone());
        changes.push(format!("title -> {title}"));
    }
    if let Some(artist) = &fields.artist {
        tag.set_artist(artist.clone());
        changes.push(format!("artist -> {artist}"));
    }
    if let Some(album) = &fields.album {
        tag.set_album(album.clone());
        changes.push(format!("album -> {album}"));
    }
    if let Some(genre) = &fields.genre {
        tag.set_genre(genre.clone());
        changes.push(format!("genre -> {genre}"));
    }
    if let Some(year) = &fields.year {
        if let Ok(y) = year.parse::<u32>() {
            tag.set_year(y);
            changes.push(format!("year -> {year}"));
        }
    }
    if let Some(album_artist) = &fields.album_artist {
        tag.insert(lofty::tag::TagItem::new(
            ItemKey::AlbumArtist,
            lofty::tag::ItemValue::Text(album_artist.clone()),
        ));
        changes.push(format!("album_artist -> {album_artist}"));
    }

    tag.save_to_path(path, lofty::config::WriteOptions::default())
        .map_err(|e| format!("Failed to save {path}: {e}"))?;
    Ok(changes)
}

async fn batch_edit_tags(
    State(state): State<AppState>,
    Json(body): Json<BatchEditRequest>,
) -> impl IntoResponse {
    let mut results: Vec<Value> = Vec::new();
    let mut success_count = 0;
    let mut error_count = 0;

    for track_id in &body.track_ids {
        let Some((path, info)) = get_track_path(&state, *track_id) else {
            results.push(json!({"track_id": track_id, "error": "Track not found"}));
            error_count += 1;
            continue;
        };
        match apply_tags_to_file(&path, &body.fields) {
            Ok(changes) => {
                // Update DB
                update_track_db(&state, *track_id, &body.fields);
                results.push(json!({
                    "track_id": track_id,
                    "path": info["path"],
                    "changes": changes,
                    "success": true,
                }));
                success_count += 1;
            }
            Err(e) => {
                results.push(json!({"track_id": track_id, "error": e}));
                error_count += 1;
            }
        }
    }

    Json(json!({
        "total": body.track_ids.len(),
        "success": success_count,
        "errors": error_count,
        "results": results,
    }))
    .into_response()
}

fn update_track_db(state: &AppState, track_id: i64, fields: &BatchFields) {
    let conn = state.db.connection().lock().unwrap();
    if let Some(title) = &fields.title {
        conn.execute(
            "UPDATE tracks SET title = ?1 WHERE id = ?2",
            rusqlite::params![title, track_id],
        )
        .ok();
    }
    if let Some(artist) = &fields.artist {
        conn.execute(
            "UPDATE tracks SET artist_name = ?1 WHERE id = ?2",
            rusqlite::params![artist, track_id],
        )
        .ok();
    }
    if let Some(album) = &fields.album {
        conn.execute(
            "UPDATE tracks SET album_title = ?1 WHERE id = ?2",
            rusqlite::params![album, track_id],
        )
        .ok();
    }
    if let Some(genre) = &fields.genre {
        conn.execute(
            "UPDATE tracks SET genre = ?1 WHERE id = ?2",
            rusqlite::params![genre, track_id],
        )
        .ok();
    }
    if let Some(year) = &fields.year {
        conn.execute(
            "UPDATE tracks SET year = ?1 WHERE id = ?2",
            rusqlite::params![year, track_id],
        )
        .ok();
    }
}

async fn preview_batch_edit(
    State(state): State<AppState>,
    Json(body): Json<BatchEditRequest>,
) -> impl IntoResponse {
    let mut previews: Vec<Value> = Vec::new();

    for track_id in &body.track_ids {
        let Some((_path, info)) = get_track_path(&state, *track_id) else {
            previews.push(json!({"track_id": track_id, "error": "Track not found"}));
            continue;
        };
        let mut changes: Vec<Value> = Vec::new();
        if let Some(title) = &body.fields.title {
            changes.push(json!({"field": "title", "old": info["title"], "new": title}));
        }
        if let Some(artist) = &body.fields.artist {
            changes.push(json!({"field": "artist", "old": info["artist_name"], "new": artist}));
        }
        if let Some(album) = &body.fields.album {
            changes.push(json!({"field": "album", "old": info["album_title"], "new": album}));
        }
        if let Some(genre) = &body.fields.genre {
            changes.push(json!({"field": "genre", "old": info["genre"], "new": genre}));
        }
        if let Some(year) = &body.fields.year {
            changes.push(json!({"field": "year", "old": info["year"], "new": year}));
        }
        previews.push(json!({
            "track_id": track_id,
            "path": info["path"],
            "changes": changes,
        }));
    }

    Json(json!({
        "dry_run": true,
        "total": body.track_ids.len(),
        "previews": previews,
    }))
    .into_response()
}

async fn auto_number_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();
    let tracks: Vec<(i64, String, Option<String>)> = conn
        .prepare("SELECT id, path, title FROM tracks WHERE album_id = ?1 ORDER BY path ASC")
        .and_then(|mut stmt| {
            stmt.query_map([album_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    if tracks.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Album not found or has no tracks"})),
        )
            .into_response();
    }

    let mut results: Vec<Value> = Vec::new();
    for (i, (track_id, path, title)) in tracks.iter().enumerate() {
        let track_num = (i + 1) as u32;
        // Write track number to file
        let file_result = (|| -> Result<(), String> {
            use lofty::file::TaggedFileExt;
            use lofty::tag::{Accessor, TagExt};

            let mut tagged = lofty::read_from_path(path).map_err(|e| format!("Read error: {e}"))?;
            let tag = tagged.primary_tag_mut().ok_or("No tag")?;
            tag.set_track(track_num);
            tag.save_to_path(path, lofty::config::WriteOptions::default())
                .map_err(|e| format!("Write error: {e}"))?;
            Ok(())
        })();

        // Update DB
        let conn = state.db.connection().lock().unwrap();
        conn.execute(
            "UPDATE tracks SET track_number = ?1 WHERE id = ?2",
            rusqlite::params![track_num, track_id],
        )
        .ok();
        drop(conn);

        results.push(json!({
            "track_id": track_id,
            "title": title,
            "track_number": track_num,
            "file_written": file_result.is_ok(),
            "error": file_result.err(),
        }));
    }

    Json(json!({
        "album_id": album_id,
        "tracks_numbered": results.len(),
        "results": results,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SetGenreBody {
    genre: String,
}

async fn set_album_genre(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
    Json(body): Json<SetGenreBody>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();
    let paths: Vec<(i64, String)> = conn
        .prepare("SELECT id, path FROM tracks WHERE album_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map([album_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    // Update DB for all tracks
    conn.execute(
        "UPDATE tracks SET genre = ?1 WHERE album_id = ?2",
        rusqlite::params![body.genre, album_id],
    )
    .ok();
    drop(conn);

    let mut file_errors: Vec<Value> = Vec::new();
    for (track_id, path) in &paths {
        let result = (|| -> Result<(), String> {
            use lofty::file::TaggedFileExt;
            use lofty::tag::{Accessor, TagExt};

            let mut tagged = lofty::read_from_path(path).map_err(|e| format!("Read error: {e}"))?;
            let tag = tagged.primary_tag_mut().ok_or("No tag")?;
            tag.set_genre(body.genre.clone());
            tag.save_to_path(path, lofty::config::WriteOptions::default())
                .map_err(|e| format!("Write error: {e}"))?;
            Ok(())
        })();
        if let Err(e) = result {
            file_errors.push(json!({"track_id": track_id, "error": e}));
        }
    }

    Json(json!({
        "album_id": album_id,
        "genre": body.genre,
        "tracks_updated": paths.len(),
        "file_errors": file_errors,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SetYearBody {
    year: String,
}

async fn set_album_year(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
    Json(body): Json<SetYearBody>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();
    let paths: Vec<(i64, String)> = conn
        .prepare("SELECT id, path FROM tracks WHERE album_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map([album_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    conn.execute(
        "UPDATE tracks SET year = ?1 WHERE album_id = ?2",
        rusqlite::params![body.year, album_id],
    )
    .ok();
    drop(conn);

    let year_num: Option<u32> = body.year.parse().ok();
    let mut file_errors: Vec<Value> = Vec::new();
    for (track_id, path) in &paths {
        let result = (|| -> Result<(), String> {
            use lofty::file::TaggedFileExt;
            use lofty::tag::{Accessor, TagExt};

            let mut tagged = lofty::read_from_path(path).map_err(|e| format!("Read error: {e}"))?;
            let tag = tagged.primary_tag_mut().ok_or("No tag")?;
            if let Some(y) = year_num {
                tag.set_year(y);
            }
            tag.save_to_path(path, lofty::config::WriteOptions::default())
                .map_err(|e| format!("Write error: {e}"))?;
            Ok(())
        })();
        if let Err(e) = result {
            file_errors.push(json!({"track_id": track_id, "error": e}));
        }
    }

    Json(json!({
        "album_id": album_id,
        "year": body.year,
        "tracks_updated": paths.len(),
        "file_errors": file_errors,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RenamePatternBody {
    track_ids: Vec<i64>,
    /// Pattern with placeholders: {track}, {artist}, {title}, {album}, {year}
    pattern: String,
    dry_run: Option<bool>,
}

async fn rename_by_pattern(
    State(state): State<AppState>,
    Json(body): Json<RenamePatternBody>,
) -> impl IntoResponse {
    let dry_run = body.dry_run.unwrap_or(false);
    let mut results: Vec<Value> = Vec::new();

    for track_id in &body.track_ids {
        let conn = state.db.connection().lock().unwrap();
        let track_info: Option<(String, Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>)> = conn
            .prepare("SELECT path, title, artist_name, album_title, track_number, year FROM tracks WHERE id = ?1")
            .ok()
            .and_then(|mut stmt| {
                stmt.query_row([track_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
                })
                .ok()
            });
        drop(conn);

        let Some((path, title, artist, album, track_num, year)) = track_info else {
            results.push(json!({"track_id": track_id, "error": "Track not found"}));
            continue;
        };

        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("flac");
        let parent = std::path::Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let new_name = body
            .pattern
            .replace("{track}", &format!("{:02}", track_num.unwrap_or(0)))
            .replace("{artist}", artist.as_deref().unwrap_or("Unknown"))
            .replace("{title}", title.as_deref().unwrap_or("Unknown"))
            .replace("{album}", album.as_deref().unwrap_or("Unknown"))
            .replace("{year}", year.as_deref().unwrap_or(""));

        // Sanitize filename
        let new_name: String = new_name
            .chars()
            .map(|c| if "/<>:\"\\|?*".contains(c) { '_' } else { c })
            .collect();
        let new_path = format!("{parent}/{new_name}.{ext}");

        if dry_run {
            results.push(json!({
                "track_id": track_id,
                "old_path": path,
                "new_path": new_path,
                "dry_run": true,
            }));
        } else if path != new_path {
            match std::fs::rename(&path, &new_path) {
                Ok(()) => {
                    let conn = state.db.connection().lock().unwrap();
                    conn.execute(
                        "UPDATE tracks SET path = ?1 WHERE id = ?2",
                        rusqlite::params![new_path, track_id],
                    )
                    .ok();
                    drop(conn);
                    results.push(json!({
                        "track_id": track_id,
                        "old_path": path,
                        "new_path": new_path,
                        "renamed": true,
                    }));
                }
                Err(e) => {
                    results.push(json!({
                        "track_id": track_id,
                        "error": format!("Rename failed: {e}"),
                    }));
                }
            }
        } else {
            results.push(json!({
                "track_id": track_id,
                "old_path": path,
                "new_path": new_path,
                "renamed": false,
                "reason": "same path",
            }));
        }
    }

    Json(json!({
        "dry_run": dry_run,
        "total": body.track_ids.len(),
        "results": results,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct FixEncodingBody {
    track_ids: Vec<i64>,
}

async fn fix_encoding(
    State(state): State<AppState>,
    Json(body): Json<FixEncodingBody>,
) -> impl IntoResponse {
    let mut results: Vec<Value> = Vec::new();

    for track_id in &body.track_ids {
        let Some((path, info)) = get_track_path(&state, *track_id) else {
            results.push(json!({"track_id": track_id, "error": "Track not found"}));
            continue;
        };

        let mut fixed_fields: Vec<Value> = Vec::new();

        // Check each text field for mojibake (Latin1 bytes interpreted as UTF-8)
        for field_name in &["title", "artist_name", "album_title", "genre"] {
            if let Some(val) = info[field_name].as_str() {
                if let Some(fixed) = try_fix_mojibake(val) {
                    fixed_fields.push(json!({
                        "field": field_name,
                        "old": val,
                        "new": fixed,
                    }));
                }
            }
        }

        if fixed_fields.is_empty() {
            results.push(json!({
                "track_id": track_id,
                "fixed": false,
                "message": "No encoding issues detected",
            }));
        } else {
            // Apply fixes to file and DB
            let fields = BatchFields {
                title: fixed_fields
                    .iter()
                    .find(|f| f["field"] == "title")
                    .and_then(|f| f["new"].as_str().map(String::from)),
                artist: fixed_fields
                    .iter()
                    .find(|f| f["field"] == "artist_name")
                    .and_then(|f| f["new"].as_str().map(String::from)),
                album: fixed_fields
                    .iter()
                    .find(|f| f["field"] == "album_title")
                    .and_then(|f| f["new"].as_str().map(String::from)),
                genre: fixed_fields
                    .iter()
                    .find(|f| f["field"] == "genre")
                    .and_then(|f| f["new"].as_str().map(String::from)),
                year: None,
                album_artist: None,
            };
            let file_result = apply_tags_to_file(&path, &fields);
            update_track_db(&state, *track_id, &fields);
            results.push(json!({
                "track_id": track_id,
                "fixed": true,
                "fixes": fixed_fields,
                "file_written": file_result.is_ok(),
                "error": file_result.err(),
            }));
        }
    }

    Json(json!({
        "total": body.track_ids.len(),
        "results": results,
    }))
    .into_response()
}

/// Attempt to fix mojibake: if string contains typical Latin1-as-UTF8 artifacts,
/// re-encode from Latin1 to UTF-8.
fn try_fix_mojibake(s: &str) -> Option<String> {
    // Common mojibake patterns: multi-byte sequences that look like accented Latin1 chars
    let suspicious = s.contains('\u{00c3}') // A-tilde (common prefix of UTF-8 encoded Latin1)
        || s.contains('\u{00c2}') // A-circumflex
        || s.contains("\u{00c3}\u{00a9}") // e-acute as mojibake
        || s.contains("\u{00c3}\u{00a8}"); // e-grave as mojibake

    if !suspicious {
        return None;
    }

    // Try interpreting the UTF-8 bytes as Latin1 and re-decode
    let bytes: Vec<u8> = s
        .chars()
        .filter_map(|c| {
            let cp = c as u32;
            if cp <= 0xFF { Some(cp as u8) } else { None }
        })
        .collect();

    if bytes.len() != s.chars().count() {
        return None; // Has chars outside Latin1 range, skip
    }

    match String::from_utf8(bytes) {
        Ok(fixed) if fixed != s => Some(fixed),
        _ => None,
    }
}

#[derive(Deserialize)]
struct StripTagsBody {
    track_ids: Vec<i64>,
    /// Tag keys to keep. Everything else is removed.
    keep: Option<Vec<String>>,
}

async fn strip_extra_tags(
    State(state): State<AppState>,
    Json(body): Json<StripTagsBody>,
) -> impl IntoResponse {
    let keep_set: std::collections::HashSet<String> = body
        .keep
        .unwrap_or_else(|| {
            vec![
                "TITLE",
                "ARTIST",
                "ALBUM",
                "ALBUMARTIST",
                "GENRE",
                "DATE",
                "TRACKNUMBER",
                "DISCNUMBER",
                "COMMENT",
            ]
            .into_iter()
            .map(String::from)
            .collect()
        })
        .into_iter()
        .map(|s| s.to_uppercase())
        .collect();

    let mut results: Vec<Value> = Vec::new();

    for track_id in &body.track_ids {
        let Some((path, _info)) = get_track_path(&state, *track_id) else {
            results.push(json!({"track_id": track_id, "error": "Track not found"}));
            continue;
        };

        let result = (|| -> Result<Vec<String>, String> {
            use lofty::file::TaggedFileExt;
            use lofty::tag::TagExt;

            let mut tagged =
                lofty::read_from_path(&path).map_err(|e| format!("Read error: {e}"))?;
            let tag = tagged.primary_tag_mut().ok_or("No tag")?;

            // Collect keys to remove
            let to_remove: Vec<lofty::tag::ItemKey> = tag
                .items()
                .filter(|item| {
                    let key_str = format!("{:?}", item.key());
                    !keep_set.contains(&key_str.to_uppercase())
                })
                .map(|item| item.key().clone())
                .collect();

            let removed: Vec<String> = to_remove.iter().map(|k| format!("{:?}", k)).collect();

            for key in &to_remove {
                tag.remove_key(key);
            }

            tag.save_to_path(&path, lofty::config::WriteOptions::default())
                .map_err(|e| format!("Write error: {e}"))?;
            Ok(removed)
        })();

        match result {
            Ok(removed) => {
                results.push(json!({
                    "track_id": track_id,
                    "stripped": removed.len(),
                    "removed_keys": removed,
                }));
            }
            Err(e) => {
                results.push(json!({"track_id": track_id, "error": e}));
            }
        }
    }

    Json(json!({
        "total": body.track_ids.len(),
        "results": results,
    }))
    .into_response()
}
