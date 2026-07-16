#![allow(dead_code)]
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::routes::active_profile::DEFAULT_PROFILE_ID;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/services", get(list_services))
        .route("/transfer", post(transfer_playlist))
        .route("/batch-transfer", post(batch_transfer))
        .route("/history", get(transfer_history))
        .route("/history/{id}", get(transfer_history_detail))
        .route("/links", get(list_links).post(create_link))
        .route("/links/{id}", axum::routing::delete(delete_link))
        .route("/links/{id}/sync", post(sync_link))
        .route("/backup", post(backup_playlists))
        .route("/backups", get(list_backups))
        .route("/backups/{id}", get(get_backup).delete(delete_backup))
        .route("/backups/{id}/restore", post(restore_backup))
        .route("/merge", post(merge_playlists))
        .route("/export", post(export_playlists))
        .route("/import", post(import_playlists))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_json_setting(settings: &SettingsRepo, key: &str) -> Vec<Value> {
    settings
        .get(key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json_setting(settings: &SettingsRepo, key: &str, data: &[Value]) {
    settings
        .set(
            key,
            &serde_json::to_string(data).unwrap_or_else(|_| "[]".into()),
        )
        .ok();
}

fn next_id(items: &[Value]) -> i64 {
    items
        .iter()
        .filter_map(|v| v.get("id").and_then(|id| id.as_i64()))
        .max()
        .unwrap_or(0)
        + 1
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO-8601 UTC timestamp without chrono dependency
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Approximate date from days since epoch (good enough for timestamps)
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// List streaming services with their playlist capabilities.
async fn list_services(State(state): State<AppState>) -> Json<Value> {
    let registry = state.services.lock().await;
    let status_all = registry.status_all().await;

    let mut services = serde_json::Map::new();
    for svc_status in &status_all {
        let name = svc_status
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let authenticated = svc_status
            .get("authenticated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let write = if let Some(svc) = registry.get(name) {
            svc.lock().await.supports_write()
        } else {
            false
        };
        services.insert(
            name.to_string(),
            json!({
                "authenticated": authenticated,
                "supports_write": write,
            }),
        );
    }
    drop(registry);
    services.insert(
        "local".to_string(),
        json!({ "authenticated": true, "supports_write": true }),
    );
    Json(json!(services))
}

// ---------------------------------------------------------------------------
// Transfer
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TransferRequest {
    source_service: String,
    source_playlist_id: String,
    target_service: String,
    #[serde(rename = "name")]
    target_name: Option<String>,
    match_threshold: Option<f64>,
    #[serde(default)]
    include_approximate: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default = "default_true")]
    create_on_target: bool,
}

fn default_true() -> bool {
    true
}

async fn transfer_playlist(
    State(state): State<AppState>,
    Json(body): Json<TransferRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    // Resolve source tracks
    let (source_tracks, source_name) = if body.source_service == "local" {
        let playlist_id: i64 = body.source_playlist_id.parse().unwrap_or(0);
        let track_ids = playlist_repo.get_track_ids(playlist_id).unwrap_or_default();
        let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
        let source_tracks: Vec<Value> = tracks
            .iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist_name": t.artist_name,
                    "album_title": t.album_title,
                    "duration_ms": t.duration_ms,
                })
            })
            .collect();
        let name = playlist_repo
            .get(playlist_id)
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_else(|| "Local Playlist".into());
        (source_tracks, name)
    } else {
        // Streaming service source — fetch playlist tracks via the service
        let registry = state.services.lock().await;
        let svc_arc = match registry.get(&body.source_service) {
            Some(arc) => arc,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": format!("Source service '{}' not found", body.source_service)})),
                )
                    .into_response()
            }
        };
        drop(registry);

        let svc = svc_arc.lock().await;
        let playlist_tracks = match svc.get_playlist_tracks(&body.source_playlist_id).await {
            Ok(tracks) => tracks,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"detail": format!("Failed to load source playlist: {e}")})),
                )
                    .into_response();
            }
        };

        let source_tracks: Vec<Value> = playlist_tracks
            .iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist_name": t.artist,
                    "album_title": t.album.as_deref().unwrap_or(""),
                    "duration_ms": t.duration_ms,
                    "source_id": t.id,
                    "isrc": "",
                })
            })
            .collect();

        // Get source playlist name
        let source_name = match svc.get_user_playlists().await {
            Ok(playlists) => playlists
                .iter()
                .find(|p| p.id == body.source_playlist_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| body.source_playlist_id.clone()),
            Err(_) => body.source_playlist_id.clone(),
        };

        (source_tracks, source_name)
    };

    let total = source_tracks.len();
    let target_name = body
        .target_name
        .unwrap_or_else(|| format!("{source_name} (transferred)"));
    let _threshold = body.match_threshold.unwrap_or(0.8);

    // Match tracks on target
    let mut matched = 0usize;
    let approximate = 0usize;
    let mut not_found = 0usize;
    let mut matched_track_ids: Vec<i64> = Vec::new();
    let mut track_details: Vec<Value> = Vec::new();

    for track in &source_tracks {
        let title = track.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let artist = track
            .get("artist_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if body.target_service == "local" {
            let query = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{title} {artist}")
            };
            let results = track_repo.search(&query, 5).unwrap_or_default();
            if let Some(best) = results.first() {
                if let Some(id) = best.id {
                    matched_track_ids.push(id);
                    matched += 1;
                    track_details.push(json!({
                        "source_title": title,
                        "source_artist": artist,
                        "matched_title": best.title,
                        "matched_artist": best.artist_name,
                        "status": "matched",
                    }));
                    continue;
                }
            }
            not_found += 1;
            track_details.push(json!({
                "source_title": title,
                "source_artist": artist,
                "status": "not_found",
            }));
        } else {
            // Search on target streaming service
            let registry = state.services.lock().await;
            let svc_arc = match registry.get(&body.target_service) {
                Some(arc) => arc,
                None => {
                    not_found += 1;
                    continue;
                }
            };
            drop(registry);

            let svc = svc_arc.lock().await;
            let query = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{title} {artist}")
            };
            match svc.search(&query, 5).await {
                Ok(results) => {
                    if let Some(first) = results.tracks.first() {
                        matched += 1;
                        track_details.push(json!({
                            "source_title": title,
                            "source_artist": artist,
                            "matched_title": first.title,
                            "matched_artist": first.artist,
                            "matched_id": first.id,
                            "status": "matched",
                        }));
                    } else {
                        not_found += 1;
                        track_details.push(json!({
                            "source_title": title,
                            "source_artist": artist,
                            "status": "not_found",
                        }));
                    }
                }
                Err(_) => {
                    not_found += 1;
                    track_details.push(json!({
                        "source_title": title,
                        "source_artist": artist,
                        "status": "not_found",
                    }));
                }
            }
        }
    }

    // Create playlist on target
    let mut target_playlist_id: Option<i64> = None;
    let mut remote_playlist_id: Option<String> = None;
    if !body.dry_run && matched > 0 {
        if body.target_service == "local" {
            if let Ok(id) =
                playlist_repo.create(&target_name, Some("Transferred playlist"), DEFAULT_PROFILE_ID)
            {
                playlist_repo.add_tracks(id, &matched_track_ids, None).ok();
                target_playlist_id = Some(id);
            }
        } else {
            // Create playlist on streaming service target
            let matched_ids: Vec<String> = track_details
                .iter()
                .filter(|t| t["status"].as_str() == Some("matched"))
                .filter_map(|t| t["matched_id"].as_str().map(|s| s.to_string()))
                .collect();
            if !matched_ids.is_empty() {
                let registry = state.services.lock().await;
                if let Some(svc_arc) = registry.get(&body.target_service) {
                    drop(registry);
                    let svc = svc_arc.lock().await;
                    match svc
                        .create_playlist(&target_name, Some("Created by Tune"))
                        .await
                    {
                        Ok(pid) => match svc.add_tracks_to_playlist(&pid, &matched_ids).await {
                            Ok(added) => {
                                tracing::info!(
                                    service = %body.target_service,
                                    playlist_id = %pid,
                                    added,
                                    "playlist_created_on_service"
                                );
                                remote_playlist_id = Some(pid);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "add_tracks_to_service_playlist_failed");
                                remote_playlist_id = Some(pid);
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                service = %body.target_service,
                                error = %e,
                                "create_playlist_on_service_failed (service may not support write)"
                            );
                        }
                    }
                }
            }
        }
    }

    // Record in transfer history
    let mut history = load_json_setting(&settings, "playlist_transfer_history");
    let history_id = next_id(&history);
    let entry = json!({
        "id": history_id,
        "operation": "transfer",
        "source_service": body.source_service,
        "source_playlist_name": source_name,
        "target_service": body.target_service,
        "target_playlist_name": target_name,
        "total_tracks": total,
        "matched": matched,
        "approximate": approximate,
        "not_found": not_found,
        "status": if body.dry_run { "dry_run" } else { "completed" },
        "started_at": now_iso(),
        "completed_at": now_iso(),
        "details": track_details,
    });
    history.push(entry);
    save_json_setting(&settings, "playlist_transfer_history", &history);

    Json(json!({
        "transfer_id": history_id,
        "source_service": body.source_service,
        "source_playlist_name": source_name,
        "target_service": body.target_service,
        "target_playlist_name": target_name,
        "target_playlist_id": target_playlist_id,
        "remote_playlist_id": remote_playlist_id,
        "total_tracks": total,
        "matched": matched,
        "approximate": approximate,
        "not_found": not_found,
        "match_rate": if total > 0 { matched as f64 / total as f64 } else { 0.0 },
        "dry_run": body.dry_run,
        "status": if body.dry_run { "dry_run" } else { "completed" },
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Batch Transfer
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BatchTransferRequest {
    source_service: String,
    target_service: String,
    playlist_ids: Option<Vec<String>>,
    match_threshold: Option<f64>,
}

async fn batch_transfer(
    State(state): State<AppState>,
    Json(body): Json<BatchTransferRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Get source playlists
    let registry = state.services.lock().await;
    let svc_arc = match registry.get(&body.source_service) {
        Some(arc) => arc,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": format!("Source '{}' not found", body.source_service)})),
            )
                .into_response();
        }
    };
    drop(registry);

    let svc = svc_arc.lock().await;
    let all_playlists = svc.get_user_playlists().await.unwrap_or_default();
    drop(svc);

    let playlists_to_transfer: Vec<_> = if let Some(ref ids) = body.playlist_ids {
        all_playlists
            .iter()
            .filter(|p| ids.contains(&p.id))
            .collect()
    } else {
        all_playlists.iter().collect()
    };

    let total = playlists_to_transfer.len();

    // Record batch in history
    let mut history = load_json_setting(&settings, "playlist_transfer_history");
    let batch_id = next_id(&history);
    history.push(json!({
        "id": batch_id,
        "operation": "batch_transfer",
        "source_service": body.source_service,
        "source_playlist_name": format!("{} playlists", total),
        "target_service": body.target_service,
        "target_playlist_name": "",
        "total_tracks": 0,
        "matched": 0,
        "approximate": 0,
        "not_found": 0,
        "status": "started",
        "started_at": now_iso(),
    }));
    save_json_setting(&settings, "playlist_transfer_history", &history);

    Json(json!({
        "batch_id": batch_id,
        "total_playlists": total,
        "status": "started",
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Transfer History
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    operation: Option<String>,
}

async fn transfer_history(
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let history = load_json_setting(&settings, "playlist_transfer_history");

    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);

    let filtered: Vec<&Value> = history
        .iter()
        .rev()
        .filter(|entry| {
            if let Some(ref op) = q.operation {
                entry
                    .get("operation")
                    .and_then(|v| v.as_str())
                    .map(|o| o == op)
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .skip(offset)
        .take(limit)
        .collect();

    // Strip details from summary view
    let summary: Vec<Value> = filtered
        .iter()
        .map(|e| {
            let mut v = (*e).clone();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("details");
            }
            v
        })
        .collect();

    Json(json!(summary))
}

async fn transfer_history_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let history = load_json_setting(&settings, "playlist_transfer_history");
    let entry = history
        .iter()
        .find(|e| e.get("id").and_then(|v| v.as_i64()) == Some(id));
    match entry {
        Some(e) => Json(e.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Playlist Links
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateLinkRequest {
    local_playlist_id: i64,
    service: String,
    service_playlist_id: String,
    sync_direction: Option<String>,
    sync_interval_minutes: Option<i64>,
}

async fn list_links(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let links = load_json_setting(&settings, "playlist_links");
    Json(json!(links))
}

async fn create_link(
    State(state): State<AppState>,
    Json(body): Json<CreateLinkRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");
    let id = next_id(&links);
    let link = json!({
        "id": id,
        "local_playlist_id": body.local_playlist_id,
        "service": body.service,
        "service_playlist_id": body.service_playlist_id,
        "sync_direction": body.sync_direction.as_deref().unwrap_or("pull"),
        "sync_interval_minutes": body.sync_interval_minutes.unwrap_or(0),
        "last_synced_at": null,
        "created_at": now_iso(),
    });
    links.push(link.clone());
    save_json_setting(&settings, "playlist_links", &links);
    (StatusCode::CREATED, Json(link)).into_response()
}

async fn delete_link(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");
    let before = links.len();
    links.retain(|l| l.get("id").and_then(|v| v.as_i64()) != Some(id));
    if links.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "playlist_links", &links);
    Json(json!({"ok": true})).into_response()
}

async fn sync_link(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");

    let link = links
        .iter_mut()
        .find(|l| l.get("id").and_then(|v| v.as_i64()) == Some(id));
    let link = match link {
        Some(l) => l,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let service = link
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let service_playlist_id = link
        .get("service_playlist_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let local_playlist_id = link
        .get("local_playlist_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let direction = link
        .get("sync_direction")
        .and_then(|v| v.as_str())
        .unwrap_or("pull")
        .to_string();

    // Fetch remote playlist tracks
    let registry = state.services.lock().await;
    let svc_arc = match registry.get(&service) {
        Some(arc) => arc,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": format!("Service '{service}' not available")})),
            )
                .into_response();
        }
    };
    drop(registry);

    let svc = svc_arc.lock().await;
    let remote_tracks = svc
        .get_playlist_tracks(&service_playlist_id)
        .await
        .unwrap_or_default();
    drop(svc);

    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let local_track_ids = playlist_repo
        .get_track_ids(local_playlist_id)
        .unwrap_or_default();

    let mut added_to_local = 0i64;

    if direction == "pull" || direction == "bidirectional" {
        // Match remote tracks against local library and add missing ones
        for rt in &remote_tracks {
            let title = rt.title.as_str();
            let artist = rt.artist.as_str();
            let query = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{title} {artist}")
            };
            if let Ok(results) = track_repo.search(&query, 1) {
                if let Some(track) = results.first() {
                    if let Some(tid) = track.id {
                        if !local_track_ids.contains(&tid) {
                            playlist_repo
                                .add_tracks(local_playlist_id, &[tid], None)
                                .ok();
                            added_to_local += 1;
                        }
                    }
                }
            }
        }
    }

    // Update last_synced_at
    link["last_synced_at"] = json!(now_iso());
    save_json_setting(&settings, "playlist_links", &links);

    Json(json!({
        "link_id": id,
        "direction": direction,
        "added_to_local": added_to_local,
        "added_to_remote": 0,
        "removed_from_local": 0,
        "removed_from_remote": 0,
        "conflicts": [],
        "snapshot_saved": false,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Backup / Snapshots
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BackupRequest {
    services: Option<Vec<String>>,
    #[serde(default = "default_true")]
    include_tracks: bool,
}

async fn backup_playlists(
    State(state): State<AppState>,
    Json(body): Json<BackupRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let mut snapshots = load_json_setting(&settings, "playlist_snapshots");
    let mut total_playlists = 0usize;
    let mut total_tracks = 0usize;
    let mut service_counts = serde_json::Map::new();

    // Determine which services to back up
    let registry = state.services.lock().await;
    let status_all = registry.status_all().await;
    drop(registry);

    let service_names: Vec<String> = if let Some(ref svcs) = body.services {
        svcs.clone()
    } else {
        let mut names: Vec<String> = status_all
            .iter()
            .filter_map(|s| {
                s.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        names.push("local".to_string());
        names
    };

    for svc_name in &service_names {
        let mut svc_count = 0usize;

        if svc_name == "local" {
            let playlists = playlist_repo.list(DEFAULT_PROFILE_ID, 99999, 0).unwrap_or_default();
            for pl in &playlists {
                let pl_id = pl.id.unwrap_or(0);
                let mut tracks_data: Vec<Value> = Vec::new();
                if body.include_tracks {
                    let track_ids = playlist_repo.get_track_ids(pl_id).unwrap_or_default();
                    let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
                    tracks_data = tracks
                        .iter()
                        .map(|t| {
                            json!({
                                "title": t.title,
                                "artist_name": t.artist_name,
                                "album_title": t.album_title,
                                "duration_ms": t.duration_ms,
                            })
                        })
                        .collect();
                }
                let snap_id = next_id(&snapshots);
                snapshots.push(json!({
                    "id": snap_id,
                    "source_service": "local",
                    "source_playlist_id": pl_id.to_string(),
                    "playlist_name": pl.name,
                    "track_count": tracks_data.len(),
                    "created_at": now_iso(),
                    "snapshot_data": tracks_data,
                }));
                total_playlists += 1;
                total_tracks += tracks_data.len();
                svc_count += 1;
            }
        } else {
            // Streaming service backup
            let registry = state.services.lock().await;
            let svc_arc = match registry.get(svc_name) {
                Some(arc) => arc,
                None => continue,
            };
            drop(registry);

            let svc = svc_arc.lock().await;
            let playlists = svc.get_user_playlists().await.unwrap_or_default();
            for pl in &playlists {
                let pl_name = &pl.name;
                let source_id = &pl.id;

                let mut tracks_data: Vec<Value> = Vec::new();
                if body.include_tracks {
                    if let Ok(tracks) = svc.get_playlist_tracks(source_id).await {
                        tracks_data = tracks
                            .iter()
                            .map(|t| {
                                json!({
                                    "title": t.title,
                                    "artist_name": t.artist,
                                    "album_title": t.album.as_deref().unwrap_or(""),
                                    "duration_ms": t.duration_ms,
                                    "source_id": t.id,
                                })
                            })
                            .collect();
                    }
                }
                let snap_id = next_id(&snapshots);
                snapshots.push(json!({
                    "id": snap_id,
                    "source_service": svc_name,
                    "source_playlist_id": source_id,
                    "playlist_name": pl_name,
                    "track_count": tracks_data.len(),
                    "created_at": now_iso(),
                    "snapshot_data": tracks_data,
                }));
                total_playlists += 1;
                total_tracks += tracks_data.len();
                svc_count += 1;
            }
        }
        service_counts.insert(svc_name.clone(), json!(svc_count));
    }

    save_json_setting(&settings, "playlist_snapshots", &snapshots);

    Json(json!({
        "backup_id": snapshots.len(),
        "playlists_backed_up": total_playlists,
        "total_tracks_snapshot": total_tracks,
        "services": service_counts,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct BackupsQuery {
    service: Option<String>,
    limit: Option<usize>,
}

async fn list_backups(State(state): State<AppState>, Query(q): Query<BackupsQuery>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let limit = q.limit.unwrap_or(500);

    let filtered: Vec<Value> = snapshots
        .iter()
        .rev()
        .filter(|s| {
            if let Some(ref svc) = q.service {
                s.get("source_service")
                    .and_then(|v| v.as_str())
                    .map(|n| n == svc)
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .take(limit)
        .map(|s| {
            let mut v = s.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("snapshot_data");
            }
            v
        })
        .collect();

    Json(json!(filtered))
}

async fn get_backup(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let snap = snapshots
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_i64()) == Some(id));
    match snap {
        Some(s) => Json(s.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_backup(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut snapshots = load_json_setting(&settings, "playlist_snapshots");
    let before = snapshots.len();
    snapshots.retain(|s| s.get("id").and_then(|v| v.as_i64()) != Some(id));
    if snapshots.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "playlist_snapshots", &snapshots);
    Json(json!({"deleted": true, "id": id})).into_response()
}

#[derive(Deserialize)]
struct RestoreRequest {
    target_name: Option<String>,
    #[serde(default)]
    overwrite_existing: bool,
}

async fn restore_backup(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<RestoreRequest>>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let snap = match snapshots
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_i64()) == Some(id))
    {
        Some(s) => s.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let body = body.map(|b| b.0);
    let overwrite = body.as_ref().map(|b| b.overwrite_existing).unwrap_or(false);
    let playlist_name = snap
        .get("playlist_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Restored Playlist");
    let target_name = body
        .as_ref()
        .and_then(|b| b.target_name.as_deref())
        .unwrap_or(playlist_name);

    let snapshot_tracks: Vec<Value> = snap
        .get("snapshot_data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    // Check for existing playlist
    let existing_playlists = playlist_repo.list(DEFAULT_PROFILE_ID, 99999, 0).unwrap_or_default();
    let existing = existing_playlists.iter().find(|p| p.name == target_name);
    if existing.is_some() && !overwrite {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail": format!("Local playlist '{target_name}' already exists. Use overwrite_existing=true to replace.")})),
        )
            .into_response();
    }

    let playlist_id = if let Some(ex) = existing {
        let pid = ex.id.unwrap_or(0);
        // Clear existing tracks
        let track_ids = playlist_repo.get_track_ids(pid).unwrap_or_default();
        for (pos, _) in track_ids.iter().enumerate() {
            playlist_repo.remove_track(pid, pos as i64).ok();
        }
        pid
    } else {
        match playlist_repo.create(
            target_name,
            Some(&format!("Restored from snapshot #{id}")),
            DEFAULT_PROFILE_ID,
        ) {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": e})),
                )
                    .into_response();
            }
        }
    };

    // Match snapshot tracks against local library
    let mut matched = 0i64;
    let mut not_found = 0i64;
    let mut matched_ids: Vec<i64> = Vec::new();

    for track in &snapshot_tracks {
        let title = track.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let artist = track
            .get("artist_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if title.is_empty() {
            not_found += 1;
            continue;
        }
        let query = if artist.is_empty() {
            title.to_string()
        } else {
            format!("{title} {artist}")
        };
        if let Ok(results) = track_repo.search(&query, 1) {
            if let Some(track) = results.first() {
                if let Some(tid) = track.id {
                    matched_ids.push(tid);
                    matched += 1;
                    continue;
                }
            }
        }
        not_found += 1;
    }

    if !matched_ids.is_empty() {
        playlist_repo
            .add_tracks(playlist_id, &matched_ids, None)
            .ok();
    }

    Json(json!({
        "local_playlist_id": playlist_id,
        "name": target_name,
        "tracks_restored": matched,
        "tracks_matched": matched,
        "tracks_not_found": not_found,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MergeRequest {
    playlists: Vec<MergeSource>,
    target_name: String,
    #[serde(default = "default_true")]
    deduplicate: bool,
}

#[derive(Deserialize)]
struct MergeSource {
    service: Option<String>,
    playlist_id: String,
}

async fn merge_playlists(
    State(state): State<AppState>,
    Json(body): Json<MergeRequest>,
) -> impl IntoResponse {
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let mut all_track_ids: Vec<i64> = Vec::new();

    for source in &body.playlists {
        let service = source.service.as_deref().unwrap_or("local");
        if service == "local" {
            let playlist_id: i64 = source.playlist_id.parse().unwrap_or(0);
            let ids = playlist_repo.get_track_ids(playlist_id).unwrap_or_default();
            all_track_ids.extend(ids);
        }
        // Streaming service merge would require fetching + matching; skip for now
    }

    if body.deduplicate {
        let mut seen = std::collections::HashSet::new();
        all_track_ids.retain(|id| seen.insert(*id));
    }

    let new_id = match playlist_repo.create(
        &body.target_name,
        Some("Merged playlist"),
        DEFAULT_PROFILE_ID,
    ) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": e})),
            )
                .into_response();
        }
    };

    playlist_repo.add_tracks(new_id, &all_track_ids, None).ok();

    Json(json!({
        "playlist_id": new_id,
        "name": body.target_name,
        "total_tracks": all_track_ids.len(),
        "deduplicated": body.deduplicate,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Export / Import
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ExportRequest {
    service: String,
    playlist_id: String,
    format: Option<String>,
}

async fn export_playlists(
    State(state): State<AppState>,
    Json(body): Json<ExportRequest>,
) -> Result<impl IntoResponse, AppError> {
    let format = body.format.as_deref().unwrap_or("json");
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let (name, tracks) = if body.service == "local" {
        let playlist_id: i64 = body.playlist_id.parse().unwrap_or(0);
        let pl = playlist_repo.get(playlist_id).ok().flatten();
        let name = pl.map(|p| p.name).unwrap_or_else(|| "Playlist".into());
        let track_ids = playlist_repo.get_track_ids(playlist_id).unwrap_or_default();
        let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
        let tracks_json: Vec<Value> = tracks
            .iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist_name": t.artist_name,
                    "album_title": t.album_title,
                    "duration_ms": t.duration_ms,
                })
            })
            .collect();
        (name, tracks_json)
    } else {
        let registry = state.services.lock().await;
        let svc_arc = match registry.get(&body.service) {
            Some(arc) => arc,
            None => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": format!("Service '{}' not found", body.service)})),
                )
                    .into_response());
            }
        };
        drop(registry);

        let svc = svc_arc.lock().await;
        let name = svc
            .get_user_playlists()
            .await
            .unwrap_or_default()
            .iter()
            .find(|p| p.id == body.playlist_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Playlist".into());
        let raw_tracks = svc
            .get_playlist_tracks(&body.playlist_id)
            .await
            .unwrap_or_default();
        let tracks: Vec<Value> = raw_tracks
            .iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist_name": t.artist,
                    "album_title": t.album.as_deref().unwrap_or(""),
                    "duration_ms": t.duration_ms,
                })
            })
            .collect();
        (name, tracks)
    };

    match format {
        "csv" => {
            let mut csv = String::from("title,artist,album,duration_ms\n");
            for t in &tracks {
                let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let artist = t.get("artist_name").and_then(|v| v.as_str()).unwrap_or("");
                let album = t.get("album_title").and_then(|v| v.as_str()).unwrap_or("");
                let dur = t.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                csv.push_str(&format!(
                    "\"{}\",\"{}\",\"{}\",{}\n",
                    title.replace('"', "\"\""),
                    artist.replace('"', "\"\""),
                    album.replace('"', "\"\""),
                    dur
                ));
            }
            let filename = format!("{}.csv", name.replace(' ', "_"));
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("text/csv; charset=utf-8"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, csv).into_response())
        }
        "xspf" => {
            let mut xspf = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n\
                 <trackList>\n",
            );
            for t in &tracks {
                let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let artist = t.get("artist_name").and_then(|v| v.as_str()).unwrap_or("");
                let dur = t.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                xspf.push_str(&format!(
                    "  <track><title>{title}</title><creator>{artist}</creator><duration>{dur}</duration></track>\n"
                ));
            }
            xspf.push_str("</trackList>\n</playlist>\n");
            let filename = format!("{}.xspf", name.replace(' ', "_"));
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("application/xspf+xml; charset=utf-8"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, xspf).into_response())
        }
        _ => {
            let content = serde_json::to_string_pretty(&json!({
                "name": name,
                "tracks": tracks,
                "track_count": tracks.len(),
                "exported_at": now_iso(),
            }))
            .unwrap_or_default();
            let filename = format!("{}.json", name.replace(' ', "_"));
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, content).into_response())
        }
    }
}

#[derive(Deserialize)]
struct ImportRequest {
    name: Option<String>,
    format: Option<String>,
    tracks: Vec<ImportTrack>,
}

#[derive(Deserialize)]
struct ImportTrack {
    title: String,
    artist: Option<String>,
    album: Option<String>,
}

async fn import_playlists(
    State(state): State<AppState>,
    Json(body): Json<ImportRequest>,
) -> impl IntoResponse {
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let name = body.name.unwrap_or_else(|| "Imported Playlist".into());

    let playlist_id = match playlist_repo.create(&name, Some("Imported playlist"), DEFAULT_PROFILE_ID)
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": e})),
            )
                .into_response();
        }
    };

    let mut matched = 0i64;
    let mut not_found = 0i64;
    let mut matched_ids: Vec<i64> = Vec::new();

    for t in &body.tracks {
        let artist = t.artist.as_deref().unwrap_or("");
        let query = if artist.is_empty() {
            t.title.clone()
        } else {
            format!("{} {}", t.title, artist)
        };
        if let Ok(results) = track_repo.search(&query, 1) {
            if let Some(track) = results.first() {
                if let Some(tid) = track.id {
                    matched_ids.push(tid);
                    matched += 1;
                    continue;
                }
            }
        }
        not_found += 1;
    }

    if !matched_ids.is_empty() {
        playlist_repo
            .add_tracks(playlist_id, &matched_ids, None)
            .ok();
    }

    Json(json!({
        "playlist_id": playlist_id,
        "playlist_name": name,
        "total_tracks": body.tracks.len(),
        "matched_to_library": matched,
        "unmatched": not_found,
    }))
    .into_response()
}
