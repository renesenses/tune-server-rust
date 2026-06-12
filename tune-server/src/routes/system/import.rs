use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct ImportTrackEntry {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    file_path: Option<String>,
    duration_ms: Option<i64>,
    track_number: Option<i32>,
    genre: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImportRoonRequest {
    roon_db_path: Option<String>,
    data: Option<Vec<ImportTrackEntry>>,
}

pub(super) async fn import_roon(
    State(state): State<AppState>,
    Json(body): Json<ImportRoonRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let db = state.db.clone();
    let tid = task_id.clone();

    // Store initial task status
    let settings = SettingsRepo::new(db.clone());
    settings
        .set(
            &format!("import_task_{tid}"),
            &json!({"status": "running", "imported": 0, "skipped": 0}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        let track_repo = TrackRepo::new(db.clone());
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let settings = SettingsRepo::new(db.clone());

        let mut imported = 0i32;
        let mut skipped = 0i32;
        let mut errors = Vec::<String>::new();

        // --- Path A: direct JSON data ---
        if let Some(entries) = body.data {
            for entry in &entries {
                // Skip if file_path exists and already in DB
                if let Some(ref fp) = entry.file_path {
                    if track_repo.get_by_path(fp).ok().flatten().is_some() {
                        skipped += 1;
                        continue;
                    }
                }

                let artist_name = entry.artist.as_deref().unwrap_or("Unknown Artist");
                let artist = artist_repo.get_or_create(artist_name, None, None).ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if let Some(ref album_title) = entry.album {
                    album_repo
                        .get_or_create(album_title, artist_id.unwrap_or(0), None)
                        .ok()
                } else {
                    None
                };
                let album_id = album.as_ref().and_then(|a| a.id);

                let mut track = tune_core::db::models::Track::new(entry.title.clone());
                track.artist_id = artist_id;
                track.artist_name = entry.artist.clone();
                track.album_id = album_id;
                track.album_title = entry.album.clone();
                track.duration_ms = entry.duration_ms.unwrap_or(0);
                track.track_number = entry.track_number.unwrap_or(0);
                track.genre = entry.genre.clone();
                track.file_path = entry.file_path.clone();
                track.source = "roon_import".to_string();

                match track_repo.create(&track) {
                    Ok(_) => imported += 1,
                    Err(e) => errors.push(format!("{}: {e}", entry.title)),
                }
            }
        }
        // --- Path B: SQLite database path ---
        else if let Some(ref db_path) = body.roon_db_path {
            match rusqlite::Connection::open_with_flags(
                db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                Ok(conn) => {
                    // Roon's DB schema is proprietary; try common table/column names
                    let query = "SELECT title, artist, album, path, duration, track_number, genre \
                                 FROM tracks";
                    match conn.prepare(query) {
                        Ok(mut stmt) => {
                            let rows = stmt.query_map([], |row| {
                                Ok((
                                    row.get::<_, String>(0).unwrap_or_default(),
                                    row.get::<_, Option<String>>(1).ok().flatten(),
                                    row.get::<_, Option<String>>(2).ok().flatten(),
                                    row.get::<_, Option<String>>(3).ok().flatten(),
                                    row.get::<_, Option<i64>>(4).ok().flatten(),
                                    row.get::<_, Option<i32>>(5).ok().flatten(),
                                    row.get::<_, Option<String>>(6).ok().flatten(),
                                ))
                            });
                            if let Ok(rows) = rows {
                                for row in rows.flatten() {
                                    let (
                                        title,
                                        artist,
                                        album,
                                        file_path,
                                        duration,
                                        track_num,
                                        genre,
                                    ) = row;

                                    if let Some(ref fp) = file_path {
                                        if track_repo.get_by_path(fp).ok().flatten().is_some() {
                                            skipped += 1;
                                            continue;
                                        }
                                    }

                                    let artist_name = artist.as_deref().unwrap_or("Unknown Artist");
                                    let art =
                                        artist_repo.get_or_create(artist_name, None, None).ok();
                                    let artist_id = art.as_ref().and_then(|a| a.id);

                                    let alb = if let Some(ref album_title) = album {
                                        album_repo
                                            .get_or_create(
                                                album_title,
                                                artist_id.unwrap_or(0),
                                                None,
                                            )
                                            .ok()
                                    } else {
                                        None
                                    };
                                    let album_id = alb.as_ref().and_then(|a| a.id);

                                    let mut track = tune_core::db::models::Track::new(title);
                                    track.artist_id = artist_id;
                                    track.artist_name = artist;
                                    track.album_id = album_id;
                                    track.album_title = album;
                                    track.duration_ms = duration.unwrap_or(0);
                                    track.track_number = track_num.unwrap_or(0);
                                    track.genre = genre;
                                    track.file_path = file_path;
                                    track.source = "roon_import".to_string();

                                    match track_repo.create(&track) {
                                        Ok(_) => imported += 1,
                                        Err(e) => errors.push(e.to_string()),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            errors.push(format!("Roon DB query failed (schema may differ): {e}"));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("Cannot open Roon DB: {e}"));
                }
            }
        }

        let status = if errors.is_empty() {
            "completed"
        } else {
            "completed_with_errors"
        };
        settings
            .set(
                &format!("import_task_{tid}"),
                &json!({
                    "status": status,
                    "imported": imported,
                    "skipped": skipped,
                    "errors": errors.len(),
                    "error_details": errors.iter().take(20).collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .ok();
        tracing::info!(
            task_id = tid,
            imported,
            skipped,
            errors = errors.len(),
            "roon_import_complete"
        );
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
        })),
    )
}

#[derive(Deserialize)]
pub(super) struct ImportPlexRequest {
    plex_url: String,
    plex_token: String,
    library_id: Option<String>,
}

pub(super) async fn import_plex(
    State(state): State<AppState>,
    Json(body): Json<ImportPlexRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let db = state.db.clone();
    let plex_url = body.plex_url.trim_end_matches('/').to_string();
    let token = body.plex_token.clone();
    let library_id = body.library_id.clone();
    let tid = task_id.clone();

    let settings = SettingsRepo::new(db.clone());
    settings
        .set(
            &format!("import_task_{tid}"),
            &json!({"status": "running", "imported": 0}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let settings = SettingsRepo::new(db.clone());
        let track_repo = TrackRepo::new(db.clone());
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());

        let mut imported = 0i32;
        let mut skipped = 0i32;
        let mut errors = Vec::<String>::new();

        // Determine which sections to import
        let section_keys: Vec<String> = if let Some(ref lid) = library_id {
            vec![lid.clone()]
        } else {
            // Fetch all library sections and filter music ones
            let sections_url = format!("{plex_url}/library/sections?X-Plex-Token={token}");
            match client
                .get(&sections_url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(resp) => {
                    let data: Value = resp.json().await.unwrap_or_default();
                    data["MediaContainer"]["Directory"]
                        .as_array()
                        .map(|dirs| {
                            dirs.iter()
                                .filter(|d| d["type"].as_str() == Some("artist"))
                                .filter_map(|d| d["key"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                Err(e) => {
                    errors.push(format!("Failed to fetch Plex sections: {e}"));
                    vec![]
                }
            }
        };

        for sec_key in &section_keys {
            let tracks_url =
                format!("{plex_url}/library/sections/{sec_key}/all?type=10&X-Plex-Token={token}");
            let resp = match client
                .get(&tracks_url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("Section {sec_key}: {e}"));
                    continue;
                }
            };

            let data: Value = resp.json().await.unwrap_or_default();
            let tracks = match data["MediaContainer"]["Metadata"].as_array() {
                Some(t) => t,
                None => continue,
            };

            for plex_track in tracks {
                let title = plex_track["title"].as_str().unwrap_or("").to_string();
                if title.is_empty() {
                    continue;
                }
                let artist_name = plex_track["grandparentTitle"]
                    .as_str()
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album_title = plex_track["parentTitle"].as_str().unwrap_or("").to_string();
                let duration = plex_track["duration"].as_u64().unwrap_or(0) as i64;
                let track_num = plex_track["index"].as_u64().unwrap_or(0) as i32;

                // Extract file_path from Media array if available
                let file_path = plex_track["Media"]
                    .as_array()
                    .and_then(|media| media.first())
                    .and_then(|m| m["Part"].as_array())
                    .and_then(|parts| parts.first())
                    .and_then(|p| p["file"].as_str())
                    .map(|s| s.to_string());

                // Skip if we already have this track by file_path
                if let Some(ref fp) = file_path {
                    if track_repo.get_by_path(fp).ok().flatten().is_some() {
                        skipped += 1;
                        continue;
                    }
                }

                let artist = artist_repo.get_or_create(&artist_name, None, None).ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if !album_title.is_empty() {
                    album_repo
                        .get_or_create(&album_title, artist_id.unwrap_or(0), None)
                        .ok()
                } else {
                    None
                };
                let album_id = album.as_ref().and_then(|a| a.id);

                let mut new_track = tune_core::db::models::Track::new(title);
                new_track.artist_id = artist_id;
                new_track.artist_name = Some(artist_name);
                new_track.album_id = album_id;
                new_track.album_title = if album_title.is_empty() {
                    None
                } else {
                    Some(album_title)
                };
                new_track.duration_ms = duration;
                new_track.track_number = track_num;
                new_track.file_path = file_path;
                new_track.source = "plex_import".to_string();

                match track_repo.create(&new_track) {
                    Ok(_) => imported += 1,
                    Err(e) => errors.push(e.to_string()),
                }
            }
        }

        let status = if errors.is_empty() {
            "completed"
        } else {
            "completed_with_errors"
        };
        settings
            .set(
                &format!("import_task_{tid}"),
                &json!({
                    "status": status,
                    "imported": imported,
                    "skipped": skipped,
                    "errors": errors.len(),
                    "error_details": errors.iter().take(20).collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .ok();
        tracing::info!(
            task_id = tid,
            imported,
            skipped,
            errors = errors.len(),
            "plex_import_complete"
        );
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
        })),
    )
}

pub(super) async fn import_playlists_file() -> Json<Value> {
    let task_id = uuid_v4();
    Json(json!({
        "status": "accepted",
        "message": "Playlist file import not yet implemented (M3U/CSV)",
        "task_id": task_id,
    }))
}

pub(super) async fn import_status(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = format!("import_task_{task_id}");
    if let Some(data) = settings.get(&key).ok().flatten() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&data) {
            return Json(json!({
                "task_id": task_id,
                "status": parsed["status"],
                "imported": parsed["imported"],
                "skipped": parsed["skipped"],
                "errors": parsed["errors"],
                "error_details": parsed["error_details"],
            }));
        }
    }
    Json(json!({
        "task_id": task_id,
        "status": "unknown",
    }))
}

/// Simple UUID v4 generator (no external crate needed).
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Pseudo-random but unique enough for task IDs
    let a = (seed & 0xFFFF_FFFF) as u32;
    let b = ((seed >> 32) & 0xFFFF) as u16;
    let c = ((seed >> 48) & 0x0FFF) as u16 | 0x4000; // version 4
    let d = ((seed >> 60) & 0x3FFF) as u16 | 0x8000; // variant
    let e = (seed.wrapping_mul(6364136223846793005) & 0xFFFF_FFFF_FFFF) as u64;
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

#[derive(Deserialize)]
pub(super) struct ImportJriverRequest {
    xml_path: String,
}

pub(super) async fn import_jriver(
    State(state): State<AppState>,
    Json(body): Json<ImportJriverRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let db = state.db.clone();
    let xml_path = body.xml_path.clone();
    let event_bus = state.event_bus.clone();

    let settings = SettingsRepo::new(db.clone());
    let key = format!("import_task_{task_id}");
    settings.set(&key, "running").ok();

    tokio::spawn(async move {
        let result = parse_jriver_xml(&xml_path, &db);
        let settings = SettingsRepo::new(db);
        match result {
            Ok((imported, skipped)) => {
                settings
                    .set(&key, &format!("completed:{imported}:{skipped}"))
                    .ok();
                event_bus.emit(
                    "import.completed",
                    json!({
                        "source": "jriver", "imported": imported, "skipped": skipped,
                    }),
                );
            }
            Err(e) => {
                settings.set(&key, &format!("error:{e}")).ok();
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
            "source": "jriver",
        })),
    )
        .into_response()
}

fn parse_jriver_xml(
    xml_path: &str,
    db: &tune_core::db::sqlite::SqliteDb,
) -> Result<(usize, usize), String> {
    let content = std::fs::read_to_string(xml_path).map_err(|e| format!("read {xml_path}: {e}"))?;

    let artist_repo = ArtistRepo::new(db.clone());
    let album_repo = AlbumRepo::new(db.clone());
    let track_repo = TrackRepo::new(db.clone());

    let mut imported = 0;
    let mut skipped = 0;

    // Parse JRiver XML: <MPL><Item><Field Name="X">value</Field>...</Item></MPL>
    let mut in_item = false;
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut reader = quick_xml::Reader::from_str(&content);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Item" {
                    in_item = true;
                    fields.clear();
                } else if name == "Field" && in_item {
                    if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == b"Name")
                    {
                        let field_name = String::from_utf8_lossy(&attr.value).to_string();
                        if let Ok(quick_xml::events::Event::Text(t)) =
                            reader.read_event_into(&mut buf)
                        {
                            let decoded = t.decode().unwrap_or_default();
                            let val = match quick_xml::escape::unescape(&decoded) {
                                Ok(s) => s.to_string(),
                                Err(_) => decoded.to_string(),
                            };
                            fields.insert(field_name, val);
                        }
                    }
                }
            }
            Ok(quick_xml::events::Event::End(ref e)) => {
                if String::from_utf8_lossy(e.name().as_ref()) == "Item" && in_item {
                    in_item = false;
                    let title = fields.get("Name").cloned().unwrap_or_default();
                    if title.is_empty() {
                        skipped += 1;
                        continue;
                    }
                    let artist_name = fields
                        .get("Artist")
                        .cloned()
                        .unwrap_or_else(|| "Unknown Artist".into());
                    let album_title = fields.get("Album").cloned();
                    let file_path = fields.get("Filename").cloned();

                    // Skip if already in DB by file_path
                    if let Some(ref fp) = file_path {
                        if track_repo.get_by_path(fp).ok().flatten().is_some() {
                            skipped += 1;
                            continue;
                        }
                    }

                    let artist_id = artist_repo
                        .get_or_create(&artist_name, None, None)
                        .ok()
                        .and_then(|a| a.id);
                    let album_id = album_title.as_deref().and_then(|t| {
                        album_repo
                            .get_or_create(t, artist_id.unwrap_or(0), None)
                            .ok()
                            .and_then(|a| a.id)
                    });

                    let mut track = tune_core::db::models::Track::new(title);
                    track.artist_id = artist_id;
                    track.album_id = album_id;
                    track.file_path = file_path;
                    track.genre = fields.get("Genre").cloned();
                    track.year = fields.get("Year").and_then(|y| y.parse().ok());
                    track.source = "jriver".into();
                    track_repo.create(&track).ok();
                    imported += 1;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => return Err(format!("xml parse: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok((imported, skipped))
}
