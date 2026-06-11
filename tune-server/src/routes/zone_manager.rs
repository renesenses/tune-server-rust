use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tracing::info;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/overview", get(overview))
        .route("/zones", get(list_managed_zones))
        .route("/zones/{id}/hot-swap", post(hot_swap_zone))
        .route("/zones/{id}/mute", post(mute_zone))
        .route("/groups", get(list_groups).post(create_group))
        .route(
            "/groups/{id}",
            axum::routing::patch(update_group).delete(delete_group),
        )
        .route("/groups/{id}/volume", post(group_volume))
        .route("/groups/{id}/calibrate", post(calibrate_group))
        .route("/groups/{id}/gapless", get(gapless_config))
        .route("/groups/{id}/health", get(group_health))
        .route(
            "/profiles",
            get(list_zone_profiles).post(create_zone_profile),
        )
        .route(
            "/profiles/{id}",
            axum::routing::put(update_zone_profile).delete(delete_zone_profile),
        )
        .route("/profiles/{id}/activate", post(activate_zone_profile))
        .route("/sync/stats", get(sync_stats))
        .route("/measure-latency", post(measure_latency))
        .route(
            "/oaat-groups",
            get(list_oaat_groups).post(create_oaat_group),
        )
        .route(
            "/oaat-groups/{id}",
            get(oaat_group_status).delete(delete_oaat_group),
        )
        .route("/oaat-groups/{id}/endpoints", post(oaat_group_add_endpoint))
        .route(
            "/oaat-groups/{id}/endpoints/{ep_id}",
            axum::routing::delete(oaat_group_remove_endpoint),
        )
        .route(
            "/oaat-groups/{id}/volume",
            axum::routing::put(oaat_group_set_volume),
        )
        .route(
            "/oaat-groups/{id}/endpoints/{ep_id}/volume",
            axum::routing::put(oaat_group_set_endpoint_volume),
        )
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
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
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
// Overview
// ---------------------------------------------------------------------------

/// Aggregate overview of all zones, groups, stereo pairs, and playing status.
async fn overview(State(state): State<AppState>) -> Json<Value> {
    let zone_repo = ZoneRepo::new(state.db.clone());
    let settings = SettingsRepo::new(state.db.clone());

    // Zones with playback status
    let zones = zone_repo.list().unwrap_or_default();
    let mut zone_data = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        zone_data.push(json!({
            "id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "output_device_id": z.output_device_id,
            "volume": z.volume as f64 / 100.0,
            "muted": z.muted,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "current_track": ps.now_playing,
            "position_ms": ps.position_ms,
            "queue_length": ps.queue_length,
        }));
    }

    // Groups
    let groups = load_json_setting(&settings, "zone_groups");

    // Stereo pairs
    let stereo_pairs = load_json_setting(&settings, "stereo_pairs");

    // Summary counts
    let playing_count = zone_data
        .iter()
        .filter(|z| z.get("state").and_then(|v| v.as_str()) == Some("playing"))
        .count();

    Json(json!({
        "zones": zone_data,
        "groups": groups,
        "stereo_pairs": stereo_pairs,
        "total_zones": zones.len(),
        "total_groups": groups.len(),
        "total_stereo_pairs": stereo_pairs.len(),
        "playing_zones": playing_count,
    }))
}

// ---------------------------------------------------------------------------
// Managed Zones
// ---------------------------------------------------------------------------

async fn list_managed_zones(State(state): State<AppState>) -> Json<Value> {
    let zone_repo = ZoneRepo::new(state.db.clone());
    let zones = zone_repo.list().unwrap_or_default();
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        result.push(json!({
            "id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "output_device_id": z.output_device_id,
            "volume": z.volume as f64 / 100.0,
            "muted": z.muted,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "current_track": ps.now_playing,
        }));
    }
    Json(json!(result))
}

// ---------------------------------------------------------------------------
// Hot-Swap
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HotSwapRequest {
    output_device_id: String,
    output_type: Option<String>,
}

/// Change the output device for a zone, optionally while it is playing.
async fn hot_swap_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<HotSwapRequest>,
) -> impl IntoResponse {
    let zone_repo = ZoneRepo::new(state.db.clone());

    // Verify zone exists
    let zone = match zone_repo.get(id) {
        Ok(Some(z)) => z,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let old_device = zone.output_device_id.clone();

    // Update the output device
    if let Err(e) = zone_repo.update_output_device(id, &body.output_device_id) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(ref ot) = body.output_type {
        zone_repo.update_output_type(id, ot).ok();
    }

    // If zone was playing, pause on old output and resume on new
    let ps = state.playback.get_state(id).await;
    let was_playing = ps.state == tune_core::playback::PlayState::Playing;

    if was_playing {
        // Stop playback on old output
        if let Some(ref old_dev) = old_device {
            let outputs = state.outputs.lock().await;
            if let Some(output) = outputs.get(old_dev) {
                let output = output.lock().await;
                let _ = output.stop().await;
            }
        }
    }

    Json(json!({
        "zone_id": id,
        "old_device": old_device,
        "new_device": body.output_device_id,
        "was_playing": was_playing,
        "status": "swapped",
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Mute
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MuteRequest {
    muted: bool,
}

async fn mute_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<MuteRequest>,
) -> impl IntoResponse {
    let zone_repo = ZoneRepo::new(state.db.clone());

    // Persist to DB
    if let Err(e) = zone_repo.update_muted(id, body.muted) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    // Forward to the output device (Squeezebox LMS, DLNA, etc.)
    let device_id = zone_repo
        .get(id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);
    state
        .orchestrator
        .set_mute(id, body.muted, device_id.as_deref())
        .await;

    Json(json!({ "zone_id": id, "muted": body.muted })).into_response()
}

// ---------------------------------------------------------------------------
// Groups (delegating to existing zone_groups settings)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateGroupRequest {
    name: String,
    zone_ids: Vec<i64>,
}

async fn list_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let groups = load_json_setting(&settings, "zone_groups");
    Json(json!(groups))
}

async fn create_group(
    State(state): State<AppState>,
    Json(body): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut groups = load_json_setting(&settings, "zone_groups");
    let id = next_id(&groups);
    let group = json!({
        "id": id,
        "name": body.name,
        "zone_ids": body.zone_ids,
        "created_at": now_iso(),
    });
    groups.push(group.clone());
    save_json_setting(&settings, "zone_groups", &groups);
    (StatusCode::CREATED, Json(group)).into_response()
}

#[derive(Deserialize)]
struct UpdateGroupRequest {
    name: Option<String>,
    zone_ids: Option<Vec<i64>>,
}

async fn update_group(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateGroupRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut groups = load_json_setting(&settings, "zone_groups");

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(id));
    match idx {
        Some(i) => {
            if let Some(ref name) = body.name {
                groups[i]["name"] = json!(name);
            }
            if let Some(ref zone_ids) = body.zone_ids {
                groups[i]["zone_ids"] = json!(zone_ids);
            }
            let result = groups[i].clone();
            save_json_setting(&settings, "zone_groups", &groups);
            Json(result).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_group(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut groups = load_json_setting(&settings, "zone_groups");
    let before = groups.len();
    groups.retain(|g| g.get("id").and_then(|v| v.as_i64()) != Some(id));
    if groups.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "zone_groups", &groups);
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// Group Volume
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GroupVolumeRequest {
    master_volume: Option<f64>,
    offsets: Option<std::collections::HashMap<String, f64>>,
}

async fn group_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<GroupVolumeRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let mut groups = load_json_setting(&settings, "zone_groups");

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(id));
    match idx {
        Some(i) => {
            let master = body
                .master_volume
                .unwrap_or(groups[i]["master_volume"].as_f64().unwrap_or(0.5));
            groups[i]["master_volume"] = json!(master);
            if let Some(ref offsets) = body.offsets {
                groups[i]["offsets"] = json!(offsets);
            }
            let zone_ids: Vec<i64> = groups[i]["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            save_json_setting(&settings, "zone_groups", &groups);

            // Apply volume to each zone
            let repo = ZoneRepo::new(state.db.clone());
            for zid in &zone_ids {
                let offset = body
                    .offsets
                    .as_ref()
                    .and_then(|o| o.get(&zid.to_string()))
                    .copied()
                    .unwrap_or(0.0);
                let effective = (master + offset).clamp(0.0, 1.0);
                let vol_int = (effective * 100.0) as i32;
                repo.update_volume(*zid, vol_int).ok();
                state.orchestrator.set_volume(*zid, effective, None).await;
            }

            Json(json!({"group_id": id, "master_volume": master})).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Calibrate
// ---------------------------------------------------------------------------

async fn calibrate_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let groups = load_json_setting(&settings, "zone_groups");

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(group) => {
            let zone_ids: Vec<i64> = group["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();

            let outputs = state.outputs.lock().await;
            let mut latencies = Vec::new();
            for zid in &zone_ids {
                let zone = ZoneRepo::new(state.db.clone()).get(*zid).ok().flatten();
                if let Some(ref device_id) = zone.and_then(|z| z.output_device_id) {
                    if let Some(output) = outputs.get(device_id) {
                        let output = output.lock().await;
                        let start = std::time::Instant::now();
                        let _ = output.get_status().await;
                        let rtt_ms = start.elapsed().as_millis() as i64;
                        latencies.push((*zid, rtt_ms / 2));
                    } else {
                        latencies.push((*zid, 0));
                    }
                } else {
                    latencies.push((*zid, 0));
                }
            }
            drop(outputs);

            let leader_latency = latencies.first().map(|(_, l)| *l).unwrap_or(0);
            let mut calibration = serde_json::Map::new();
            for (zid, lat) in &latencies {
                let sync_delay = leader_latency - lat;
                calibration.insert(zid.to_string(), json!(sync_delay));
            }

            Json(json!({
                "group_id": group_id,
                "calibration": calibration,
            }))
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Gapless Config
// ---------------------------------------------------------------------------

async fn gapless_config(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let groups = load_json_setting(&settings, "zone_groups");

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(group) => {
            let zone_ids: Vec<i64> = group["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();

            // Return gapless configuration for the group
            let gapless_key = format!("gapless_group_{group_id}");
            let gapless_settings: Value = settings
                .get(&gapless_key)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| {
                    json!({
                        "enabled": true,
                        "crossfade_ms": 0,
                        "prebuffer_ms": 2000,
                    })
                });

            Json(json!({
                "group_id": group_id,
                "zone_ids": zone_ids,
                "gapless": gapless_settings,
            }))
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Group Health
// ---------------------------------------------------------------------------

async fn group_health(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let groups = load_json_setting(&settings, "zone_groups");

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(group) => {
            let zone_ids: Vec<i64> = group["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();

            let repo = ZoneRepo::new(state.db);
            let mut zones_health = Vec::new();
            for zid in &zone_ids {
                let ps = state.playback.get_state(*zid).await;
                let zone = repo.get(*zid).ok().flatten();
                let name = zone
                    .map(|z| z.name)
                    .unwrap_or_else(|| format!("Zone {zid}"));
                let online =
                    ps.state != tune_core::playback::PlayState::Stopped || ps.now_playing.is_some();
                zones_health.push(json!({
                    "zone_id": zid,
                    "name": name,
                    "status": if online { "online" } else { "offline" },
                }));
            }

            Json(json!(zones_health)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Zone Profiles
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateZoneProfileRequest {
    name: String,
    zones: Option<Vec<ZoneProfileEntry>>,
    description: Option<String>,
}

#[derive(Deserialize, Clone)]
struct ZoneProfileEntry {
    zone_id: i64,
    output_device_id: Option<String>,
    output_type: Option<String>,
    volume: Option<i32>,
    muted: Option<bool>,
}

async fn list_zone_profiles(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let profiles = load_json_setting(&settings, "zone_profiles");
    Json(json!(profiles))
}

async fn create_zone_profile(
    State(state): State<AppState>,
    Json(body): Json<CreateZoneProfileRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let mut profiles = load_json_setting(&settings, "zone_profiles");
    let id = next_id(&profiles);

    // If no zones specified, snapshot current zone configuration
    let zones_config: Vec<Value> = if let Some(zones) = body.zones {
        zones
            .iter()
            .map(|z| {
                json!({
                    "zone_id": z.zone_id,
                    "output_device_id": z.output_device_id,
                    "output_type": z.output_type,
                    "volume": z.volume,
                    "muted": z.muted,
                })
            })
            .collect()
    } else {
        let zone_repo = ZoneRepo::new(state.db.clone());
        zone_repo
            .list()
            .unwrap_or_default()
            .iter()
            .map(|z| {
                json!({
                    "zone_id": z.id,
                    "output_device_id": z.output_device_id,
                    "output_type": z.output_type,
                    "volume": z.volume,
                    "muted": z.muted,
                })
            })
            .collect()
    };

    let profile = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "zones": zones_config,
        "created_at": now_iso(),
        "last_activated_at": null,
    });
    profiles.push(profile.clone());
    save_json_setting(&settings, "zone_profiles", &profiles);

    (StatusCode::CREATED, Json(profile)).into_response()
}

async fn update_zone_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<CreateZoneProfileRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut profiles = load_json_setting(&settings, "zone_profiles");

    let idx = profiles
        .iter()
        .position(|p| p.get("id").and_then(|v| v.as_i64()) == Some(id));
    match idx {
        Some(i) => {
            profiles[i]["name"] = json!(body.name);
            if let Some(ref desc) = body.description {
                profiles[i]["description"] = json!(desc);
            }
            if let Some(zones) = body.zones {
                let zones_config: Vec<Value> = zones
                    .iter()
                    .map(|z| {
                        json!({
                            "zone_id": z.zone_id,
                            "output_device_id": z.output_device_id,
                            "output_type": z.output_type,
                            "volume": z.volume,
                            "muted": z.muted,
                        })
                    })
                    .collect();
                profiles[i]["zones"] = json!(zones_config);
            }
            let result = profiles[i].clone();
            save_json_setting(&settings, "zone_profiles", &profiles);
            Json(result).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_zone_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut profiles = load_json_setting(&settings, "zone_profiles");
    let before = profiles.len();
    profiles.retain(|p| p.get("id").and_then(|v| v.as_i64()) != Some(id));
    if profiles.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "zone_profiles", &profiles);
    StatusCode::NO_CONTENT.into_response()
}

/// Activate a zone profile — apply saved zone configurations.
async fn activate_zone_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    let mut profiles = load_json_setting(&settings, "zone_profiles");

    let idx = profiles
        .iter()
        .position(|p| p.get("id").and_then(|v| v.as_i64()) == Some(id));
    let idx = match idx {
        Some(i) => i,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let zone_configs: Vec<Value> = profiles[idx]
        .get("zones")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let zone_repo = ZoneRepo::new(state.db.clone());
    let mut applied = 0usize;

    for zc in &zone_configs {
        let zone_id = match zc.get("zone_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => continue,
        };

        if let Some(device_id) = zc.get("output_device_id").and_then(|v| v.as_str()) {
            zone_repo.update_output_device(zone_id, device_id).ok();
        }
        if let Some(ot) = zc.get("output_type").and_then(|v| v.as_str()) {
            zone_repo.update_output_type(zone_id, ot).ok();
        }
        if let Some(vol) = zc.get("volume").and_then(|v| v.as_i64()) {
            zone_repo.update_volume(zone_id, vol as i32).ok();
            state
                .orchestrator
                .set_volume(zone_id, vol as f64 / 100.0, None)
                .await;
        }
        if let Some(muted) = zc.get("muted").and_then(|v| v.as_bool()) {
            zone_repo.update_muted(zone_id, muted).ok();
        }
        applied += 1;
    }

    // Update last_activated_at
    profiles[idx]["last_activated_at"] = json!(now_iso());
    save_json_setting(&settings, "zone_profiles", &profiles);

    // Store active profile id
    settings.set("active_zone_profile_id", &id.to_string()).ok();

    Json(json!({
        "profile_id": id,
        "zones_applied": applied,
        "status": "activated",
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Sync Stats
// ---------------------------------------------------------------------------

/// Return sync timing data from playback states of all zones.
async fn sync_stats(State(state): State<AppState>) -> Json<Value> {
    let zone_repo = ZoneRepo::new(state.db);
    let zones = zone_repo.list().unwrap_or_default();

    let mut zone_stats = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        zone_stats.push(json!({
            "zone_id": zone_id,
            "name": z.name,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "position_ms": ps.position_ms,
        }));
    }

    // Compute drift between playing zones
    let playing: Vec<&Value> = zone_stats
        .iter()
        .filter(|z| z.get("state").and_then(|v| v.as_str()) == Some("playing"))
        .collect();

    let max_drift_ms = if playing.len() > 1 {
        let positions: Vec<i64> = playing
            .iter()
            .filter_map(|z| z.get("position_ms").and_then(|v| v.as_i64()))
            .collect();
        let min = positions.iter().min().copied().unwrap_or(0);
        let max = positions.iter().max().copied().unwrap_or(0);
        max - min
    } else {
        0
    };

    Json(json!({
        "zones": zone_stats,
        "playing_count": playing.len(),
        "max_drift_ms": max_drift_ms,
    }))
}

// ---------------------------------------------------------------------------
// Measure Latency
// ---------------------------------------------------------------------------

/// Measure round-trip time to all zone output devices.
async fn measure_latency(State(state): State<AppState>) -> impl IntoResponse {
    let zone_repo = ZoneRepo::new(state.db);
    let zones = zone_repo.list().unwrap_or_default();
    let outputs = state.outputs.lock().await;

    let mut results = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        if let Some(ref device_id) = z.output_device_id {
            if let Some(output) = outputs.get(device_id) {
                let output = output.lock().await;
                let start = std::time::Instant::now();
                let _ = output.get_status().await;
                let rtt_ms = start.elapsed().as_millis() as u64;
                results.push(json!({
                    "zone_id": zone_id,
                    "zone_name": z.name,
                    "device_id": device_id,
                    "rtt_ms": rtt_ms,
                    "estimated_latency_ms": rtt_ms / 2,
                    "status": "reachable",
                }));
            } else {
                results.push(json!({
                    "zone_id": zone_id,
                    "zone_name": z.name,
                    "device_id": device_id,
                    "rtt_ms": null,
                    "estimated_latency_ms": null,
                    "status": "output_not_registered",
                }));
            }
        } else {
            results.push(json!({
                "zone_id": zone_id,
                "zone_name": z.name,
                "device_id": null,
                "rtt_ms": null,
                "estimated_latency_ms": null,
                "status": "no_output_assigned",
            }));
        }
    }

    Json(json!({
        "latencies": results,
        "measured_at": now_iso(),
    }))
}

// ---------------------------------------------------------------------------
// OAAT Multiroom Groups
// ---------------------------------------------------------------------------

async fn list_oaat_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let groups = load_json_setting(&settings, "oaat_groups");
    Json(json!({ "oaat_groups": groups }))
}

async fn create_oaat_group(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let name = body["name"].as_str().unwrap_or("OAAT Group");
    let endpoints: Vec<(String, u16)> = body["endpoints"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|ep| {
            let host = ep["host"].as_str()?.to_string();
            let port = ep["port"].as_u64()? as u16;
            Some((host, port))
        })
        .collect();

    if endpoints.is_empty() {
        return Json(json!({ "error": "at least one endpoint required" }));
    }

    let group_id = format!(
        "oaat-mr-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    // Register the multiroom output
    #[cfg(feature = "oaat")]
    {
        let output = tune_core::outputs::oaat::OaatMultiroomOutput::new(
            name.to_string(),
            group_id.clone(),
            endpoints.clone(),
        );
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(output));
    }

    // Persist to settings
    let settings = SettingsRepo::new(state.db);
    let mut groups = load_json_setting(&settings, "oaat_groups");
    groups.push(json!({
        "id": group_id,
        "name": name,
        "endpoints": endpoints.iter().map(|(h, p)| json!({"host": h, "port": p})).collect::<Vec<_>>(),
        "created_at": now_iso(),
    }));
    save_json_setting(&settings, "oaat_groups", &groups);

    info!(group_id = %group_id, name, endpoints = endpoints.len(), "oaat_multiroom_group_created");

    Json(json!({
        "id": group_id,
        "name": name,
        "endpoints": endpoints.len(),
        "device_id": format!("oaat-group:{group_id}"),
    }))
}

async fn delete_oaat_group(State(state): State<AppState>, Path(id): Path<String>) -> Json<Value> {
    // Remove from registry
    let device_id = format!("oaat-group:{id}");
    {
        let mut outputs = state.outputs.lock().await;
        outputs.remove(&device_id);
    }

    // Remove from settings
    let settings = SettingsRepo::new(state.db);
    let mut groups = load_json_setting(&settings, "oaat_groups");
    groups.retain(|g| g["id"].as_str() != Some(&id));
    save_json_setting(&settings, "oaat_groups", &groups);

    info!(group_id = %id, "oaat_multiroom_group_deleted");

    Json(json!({ "deleted": id }))
}

// -- Dynamic OAAT group management --

async fn oaat_group_status(State(state): State<AppState>, Path(id): Path<String>) -> Json<Value> {
    let device_id = format!("oaat-group:{id}");
    let outputs = state.outputs.lock().await;

    #[cfg(feature = "oaat")]
    if let Some(output) = outputs.get(&device_id) {
        let output = output.lock().await;
        if let Some(mr) = downcast_oaat_multiroom(&**output) {
            return Json(mr.zone_snapshot().await);
        }
    }

    Json(json!({ "error": "group not found", "id": id }))
}

#[cfg(feature = "oaat")]
async fn oaat_group_add_endpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let device_id = format!("oaat-group:{id}");
    let host = body["host"].as_str().unwrap_or("");
    let port = body["port"].as_u64().unwrap_or(9740) as u16;

    if host.is_empty() {
        return Json(json!({ "error": "host is required" }));
    }

    let outputs = state.outputs.lock().await;
    if let Some(output) = outputs.get(&device_id) {
        let output = output.lock().await;
        if let Some(mr) = downcast_oaat_multiroom(&**output) {
            match mr.add_endpoint(host, port).await {
                Ok(ep_id) => {
                    info!(group = %id, endpoint_id = %ep_id, "oaat_group_endpoint_added");
                    return Json(json!({ "endpoint_id": ep_id, "host": host, "port": port }));
                }
                Err(e) => return Json(json!({ "error": e })),
            }
        }
    }

    Json(json!({ "error": "group not found" }))
}

#[cfg(not(feature = "oaat"))]
async fn oaat_group_add_endpoint(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    Json(json!({ "error": "OAAT not compiled" }))
}

#[cfg(feature = "oaat")]
async fn oaat_group_remove_endpoint(
    State(state): State<AppState>,
    Path((id, ep_id)): Path<(String, String)>,
) -> Json<Value> {
    let device_id = format!("oaat-group:{id}");
    let outputs = state.outputs.lock().await;

    if let Some(output) = outputs.get(&device_id) {
        let output = output.lock().await;
        if let Some(mr) = downcast_oaat_multiroom(&**output) {
            let removed = mr.remove_endpoint(&ep_id).await;
            info!(group = %id, endpoint_id = %ep_id, removed, "oaat_group_endpoint_removed");
            return Json(json!({ "removed": removed, "endpoint_id": ep_id }));
        }
    }

    Json(json!({ "error": "group not found" }))
}

#[cfg(not(feature = "oaat"))]
async fn oaat_group_remove_endpoint(
    State(_state): State<AppState>,
    Path((_id, _ep_id)): Path<(String, String)>,
) -> Json<Value> {
    Json(json!({ "error": "OAAT not compiled" }))
}

#[cfg(feature = "oaat")]
async fn oaat_group_set_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let device_id = format!("oaat-group:{id}");
    let level = body["level"].as_u64().unwrap_or(100).min(100) as u8;

    let outputs = state.outputs.lock().await;
    if let Some(output) = outputs.get(&device_id) {
        let output = output.lock().await;
        if let Some(mr) = downcast_oaat_multiroom(&**output) {
            match mr.set_zone_volume(level).await {
                Ok(()) => return Json(json!({ "volume": level })),
                Err(e) => return Json(json!({ "error": e })),
            }
        }
    }

    Json(json!({ "error": "group not found" }))
}

#[cfg(not(feature = "oaat"))]
async fn oaat_group_set_volume(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    Json(json!({ "error": "OAAT not compiled" }))
}

#[cfg(feature = "oaat")]
async fn oaat_group_set_endpoint_volume(
    State(state): State<AppState>,
    Path((id, ep_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let device_id = format!("oaat-group:{id}");

    let outputs = state.outputs.lock().await;
    if let Some(output) = outputs.get(&device_id) {
        let output = output.lock().await;
        if let Some(mr) = downcast_oaat_multiroom(&**output) {
            // Support both absolute level and relative offset
            if let Some(level) = body["level"].as_u64() {
                let level = level.min(100) as u8;
                match mr.set_endpoint_volume(&ep_id, level).await {
                    Ok(()) => return Json(json!({ "endpoint_id": ep_id, "volume": level })),
                    Err(e) => return Json(json!({ "error": e })),
                }
            } else if let Some(offset) = body["offset"].as_i64() {
                let offset = offset.clamp(-100, 100) as i8;
                match mr.set_endpoint_volume_offset(&ep_id, offset).await {
                    Ok(()) => return Json(json!({ "endpoint_id": ep_id, "offset": offset })),
                    Err(e) => return Json(json!({ "error": e })),
                }
            }
            return Json(json!({ "error": "provide 'level' (0-100) or 'offset' (-100..100)" }));
        }
    }

    Json(json!({ "error": "group not found" }))
}

#[cfg(not(feature = "oaat"))]
async fn oaat_group_set_endpoint_volume(
    State(_state): State<AppState>,
    Path((_id, _ep_id)): Path<(String, String)>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    Json(json!({ "error": "OAAT not compiled" }))
}

/// Downcast an OutputTarget to OaatMultiroomOutput.
#[cfg(feature = "oaat")]
fn downcast_oaat_multiroom(
    output: &dyn tune_core::outputs::traits::OutputTarget,
) -> Option<&tune_core::outputs::oaat::OaatMultiroomOutput> {
    output
        .as_any()
        .downcast_ref::<tune_core::outputs::oaat::OaatMultiroomOutput>()
}
