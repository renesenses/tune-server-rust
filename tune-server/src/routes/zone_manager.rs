use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tracing::{info, warn};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::zones::latency::measure_control_rtt;

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

/// Contrat public d'un groupe de commandes hétérogène.
///
/// Ces groupes permettent de piloter plusieurs zones ensemble. Ils ne placent
/// pas les renderers indépendants dans un domaine d'horloge commun et ne leur
/// transmettent aucun timestamp de présentation (#2215).
fn generic_group_synchronization_contract() -> Value {
    json!({
        "supported": false,
        "transport": "independent_renderers",
        "presentation_timestamps": false,
        "render_latency_calibrated": false,
        "accuracy_claim_ms": null,
        "alternative": "oaat",
    })
}

fn generic_group_view(mut group: Value) -> Value {
    group["synchronization"] = generic_group_synchronization_contract();
    group
}

fn oaat_group_view(mut group: Value) -> Value {
    group["synchronization"] = tune_core::outputs::oaat::oaat_synchronization_contract();
    group
}

pub(crate) fn now_iso() -> String {
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
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = SettingsRepo::with_backend(state.backend.clone());

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
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
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
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());

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
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());

    let device_id = zone_repo
        .get(id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);
    match state
        .orchestrator
        .set_mute(id, body.muted, device_id.as_deref())
        .await
    {
        Ok(()) => Json(json!({ "zone_id": id, "muted": body.muted })).into_response(),
        Err(error) => crate::routes::playback::output_command_error_response(error),
    }
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let groups = load_json_setting(&settings, "zone_groups")
        .into_iter()
        .map(generic_group_view)
        .collect::<Vec<_>>();
    Json(json!(groups))
}

async fn create_group(
    State(state): State<AppState>,
    Json(body): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    (StatusCode::CREATED, Json(generic_group_view(group))).into_response()
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
            Json(generic_group_view(result)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_group(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

            // Apply volume to each zone only after its renderer accepts it.
            for zid in &zone_ids {
                let offset = body
                    .offsets
                    .as_ref()
                    .and_then(|o| o.get(&zid.to_string()))
                    .copied()
                    .unwrap_or(0.0);
                let effective = (master + offset).clamp(0.0, 1.0);
                let device_id = ZoneRepo::with_backend(state.backend.clone())
                    .get(*zid)
                    .ok()
                    .flatten()
                    .and_then(|zone| zone.output_device_id);
                if let Err(error) = state
                    .orchestrator
                    .set_volume(*zid, effective, device_id.as_deref())
                    .await
                {
                    return crate::routes::playback::output_command_error_response(error);
                }
            }

            Json(json!({"group_id": id, "master_volume": master})).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Calibrate
// ---------------------------------------------------------------------------

fn audio_calibration_unavailable_payload(group_id: i64) -> Value {
    json!({
        "error": "audio_calibration_unavailable",
        "group_id": group_id,
        "synchronization": generic_group_synchronization_contract(),
        "message": "Tune ne synchronise pas les renderers indépendants de ce groupe. Le RTT de commande ne mesure pas leur latence audio. Pour une lecture planifiée par timestamps de présentation, utilisez des points de diffusion Tune/OAAT.",
    })
}

pub(crate) fn audio_calibration_unavailable(group_id: i64) -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(audio_calibration_unavailable_payload(group_id)),
    )
        .into_response()
}

async fn calibrate_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let groups = load_json_setting(&settings, "zone_groups");

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(_) => audio_calibration_unavailable(group_id),
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

            let repo = ZoneRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let profiles = load_json_setting(&settings, "zone_profiles");
    Json(json!(profiles))
}

async fn create_zone_profile(
    State(state): State<AppState>,
    Json(body): Json<CreateZoneProfileRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
        let zone_repo = ZoneRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
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
        let device_id = zone_repo
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|zone| zone.output_device_id);
        if let Some(vol) = zc.get("volume").and_then(|v| v.as_i64()) {
            if let Err(error) = state
                .orchestrator
                .set_volume(zone_id, vol as f64 / 100.0, device_id.as_deref())
                .await
            {
                return crate::routes::playback::output_command_error_response(error);
            }
        }
        if let Some(muted) = zc.get("muted").and_then(|v| v.as_bool()) {
            if let Err(error) = state
                .orchestrator
                .set_mute(zone_id, muted, device_id.as_deref())
                .await
            {
                return crate::routes::playback::output_command_error_response(error);
            }
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
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
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
        "measurement": "reported_playback_position",
        "synchronization_guarantee": false,
        "warning": "Les positions rapportées par des renderers indépendants ne prouvent pas une restitution audio synchronisée.",
    }))
}

// ---------------------------------------------------------------------------
// Measure control RTT
// ---------------------------------------------------------------------------

/// Mesure le trajet aller-retour d'une COMMANDE vers chaque sortie.
///
/// Ce diagnostic ne prétend pas connaître la latence audio. En particulier,
/// aucun demi-RTT n'est publié comme estimation de restitution (#2215).
async fn measure_latency(State(state): State<AppState>) -> impl IntoResponse {
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = zone_repo.list().unwrap_or_default();
    let outputs = state.outputs.lock().await;

    let mut results = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        if let Some(ref device_id) = z.output_device_id {
            if let Some(output) = outputs.get(device_id) {
                let output = output.lock().await;
                match measure_control_rtt(&**output, 5).await {
                    Some(stats) => results.push(json!({
                        "zone_id": zone_id,
                        "zone_name": z.name,
                        "device_id": device_id,
                        "measurement": "control_rtt",
                        "control_rtt": {
                            "samples": stats.samples,
                            "min_ms": stats.min_ms,
                            "p50_ms": stats.p50_ms,
                            "p95_ms": stats.p95_ms,
                            "p99_ms": stats.p99_ms,
                            "max_ms": stats.max_ms,
                            "uncertainty_ms": stats.uncertainty_ms,
                        },
                        "audio_latency_ms": null,
                        "status": "reachable",
                    })),
                    None => results.push(json!({
                        "zone_id": zone_id,
                        "zone_name": z.name,
                        "device_id": device_id,
                        "measurement": "control_rtt",
                        "control_rtt": null,
                        "audio_latency_ms": null,
                        "status": "probe_failed",
                    })),
                }
            } else {
                results.push(json!({
                    "zone_id": zone_id,
                    "zone_name": z.name,
                    "device_id": device_id,
                    "measurement": "control_rtt",
                    "control_rtt": null,
                    "audio_latency_ms": null,
                    "status": "output_not_registered",
                }));
            }
        } else {
            results.push(json!({
                "zone_id": zone_id,
                "zone_name": z.name,
                "device_id": null,
                "measurement": "control_rtt",
                "control_rtt": null,
                "audio_latency_ms": null,
                "status": "no_output_assigned",
            }));
        }
    }

    Json(json!({
        "latencies": results,
        "measurement": "control_rtt",
        "audio_latency_available": false,
        "synchronization_scope": "oaat_only",
        "warning": "Le RTT de commande ne mesure pas la latence de restitution audio. Tune ne promet une planification synchronisée que pour ses points de diffusion OAAT.",
        "measured_at": now_iso(),
    }))
}

// ---------------------------------------------------------------------------
// OAAT Multiroom Groups
// ---------------------------------------------------------------------------

async fn list_oaat_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let groups = load_json_setting(&settings, "oaat_groups")
        .into_iter()
        .map(oaat_group_view)
        .collect::<Vec<_>>();
    Json(json!({ "oaat_groups": groups }))
}

/// Délai laissé à un point de diffusion pour accepter une connexion TCP.
///
/// Court volontairement : les points de diffusion sont sur le réseau local, et
/// cette sonde s'exécute pendant que l'utilisateur attend devant son écran. Un
/// appareil qui n'a pas répondu en une seconde et demie sur son propre réseau
/// ne jouera pas non plus.
const ENDPOINT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Ceux des membres qui n'acceptent pas une connexion sur le port OAAT.
///
/// Un groupe multiroom est servi par `OaatMultiroomOutput`, qui ouvre une
/// connexion TCP vers chaque membre sur le port du protocole OAAT. Un renderer
/// DLNA n'écoute rien là : la connexion est refusée, `connected` retombe à
/// zéro, et le groupe ne peut plus jamais jouer.
///
/// Rien ne vérifiait cela à la création. Cyrille Moutia a donc pu grouper deux
/// Yamaha DLNA, voir le groupe s'afficher comme actif, et chercher pendant un
/// moment ce qu'il avait mal réglé — alors que le seul endroit où l'échec
/// apparaissait était une ligne d'erreur dans un journal (#1779).
///
/// Les sondes partent en parallèle : le coût du contrôle est celui du membre
/// le plus lent, pas leur somme.
pub(crate) async fn unreachable_endpoints(endpoints: &[(String, u16)]) -> Vec<String> {
    let probes = endpoints.iter().map(|(host, port)| {
        let host = host.clone();
        let port = *port;
        async move {
            let ok = tokio::time::timeout(
                ENDPOINT_PROBE_TIMEOUT,
                tokio::net::TcpStream::connect((host.as_str(), port)),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            if ok {
                None
            } else {
                Some(format!("{host}:{port}"))
            }
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
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

    // Un groupe qui ne peut pas jouer ne doit pas pouvoir se créer (#1779).
    // Avant ce contrôle, le groupe était accepté, persisté et réenregistré à
    // chaque démarrage ; l'échec n'apparaissait que dans les journaux.
    let unreachable = unreachable_endpoints(&endpoints).await;
    if !unreachable.is_empty() {
        warn!(
            name,
            unreachable = unreachable.join(", "),
            total = endpoints.len(),
            "oaat_multiroom_group_refused_unreachable_endpoints"
        );
        return Json(json!({
            "error": format!(
                "Ces appareils ne répondent pas comme des points de diffusion Tune : {}. \
                 Le multiroom synchronisé relie des points de diffusion Tune entre eux ; \
                 un lecteur DLNA ou AirPlay ne peut pas en faire partie. \
                 Le groupe n'a pas été créé.",
                unreachable.join(", ")
            ),
            "unreachable_endpoints": unreachable,
        }));
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut groups = load_json_setting(&settings, "oaat_groups");
    groups.push(json!({
        "id": group_id,
        "name": name,
        "endpoints": endpoints.iter().map(|(h, p)| json!({"host": h, "port": p})).collect::<Vec<_>>(),
        "created_at": now_iso(),
    }));
    save_json_setting(&settings, "oaat_groups", &groups);

    info!(group_id = %group_id, name, endpoints = endpoints.len(), "oaat_multiroom_group_created");

    Json(oaat_group_view(json!({
        "id": group_id,
        "name": name,
        "endpoints": endpoints.len(),
        "device_id": format!("oaat-group:{group_id}"),
    })))
}

async fn delete_oaat_group(State(state): State<AppState>, Path(id): Path<String>) -> Json<Value> {
    // Remove from registry
    let device_id = format!("oaat-group:{id}");
    {
        let mut outputs = state.outputs.lock().await;
        outputs.remove(&device_id);
    }

    // Remove from settings
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Un port fermé sur la boucle locale : on ouvre un écouteur pour obtenir
    /// un numéro de port réellement libre, puis on le referme. Tirer un numéro
    /// au hasard donnerait un test qui échoue le jour où quelque chose écoute
    /// dessus.
    async fn port_ferme() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    #[tokio::test]
    async fn un_appareil_qui_refuse_la_connexion_est_signale() {
        // C'est le cas de Cyrille : deux Yamaha DLNA, rien à l'écoute sur le
        // port OAAT, `Connection refused` sur les deux. Le groupe était
        // pourtant créé et persisté (#1779).
        let port = port_ferme().await;
        let refuses = unreachable_endpoints(&[("127.0.0.1".to_string(), port)]).await;
        assert_eq!(
            refuses,
            vec![format!("127.0.0.1:{port}")],
            "un membre injoignable doit être nommé, pour que l'interface \
             puisse dire LEQUEL pose problème"
        );
    }

    #[tokio::test]
    async fn un_appareil_qui_accepte_la_connexion_passe() {
        // Contre-épreuve indispensable : un contrôle qui refuse tout le monde
        // « marche » aussi sur le test précédent, et casserait le multiroom
        // pour les vrais points de diffusion.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = l.accept().await;
        });
        let refuses = unreachable_endpoints(&[("127.0.0.1".to_string(), port)]).await;
        assert!(
            refuses.is_empty(),
            "un point de diffusion qui accepte la connexion ne doit pas être \
             refusé, sinon on casse le multiroom qui fonctionne : {refuses:?}"
        );
    }

    #[tokio::test]
    async fn seuls_les_membres_fautifs_sont_nommes() {
        // Le message affiché ne doit pas accuser tout le groupe quand un seul
        // membre est en cause — c'est la différence entre « votre groupe ne
        // marche pas » et « CET appareil ne peut pas en faire partie ».
        let bon = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_bon = bon.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if bon.accept().await.is_err() {
                    break;
                }
            }
        });
        let port_mauvais = port_ferme().await;
        let refuses = unreachable_endpoints(&[
            ("127.0.0.1".to_string(), port_bon),
            ("127.0.0.1".to_string(), port_mauvais),
        ])
        .await;
        assert_eq!(refuses, vec![format!("127.0.0.1:{port_mauvais}")]);
    }

    #[test]
    fn le_refus_de_calibrage_explique_la_mesure_manquante() {
        let payload = audio_calibration_unavailable_payload(42);

        assert_eq!(payload["error"], "audio_calibration_unavailable");
        assert_eq!(payload["group_id"], 42);
        assert!(payload["message"].as_str().unwrap().contains("RTT"));
        assert!(payload["message"].as_str().unwrap().contains("audio"));
        assert_eq!(payload["synchronization"]["supported"], false);
        assert_eq!(payload["synchronization"]["presentation_timestamps"], false);
        assert_eq!(payload["synchronization"]["alternative"], "oaat");
        assert!(payload["synchronization"]["accuracy_claim_ms"].is_null());
    }

    #[test]
    fn un_groupe_generique_n_est_jamais_annonce_comme_synchronise() {
        let view = generic_group_view(json!({
            "id": 7,
            "name": "Salon + cuisine",
            "zone_ids": [1, 2],
        }));

        assert_eq!(view["synchronization"]["supported"], false);
        assert_eq!(
            view["synchronization"]["transport"],
            "independent_renderers"
        );
        assert_eq!(view["synchronization"]["alternative"], "oaat");
    }

    #[test]
    fn un_groupe_oaat_annonce_le_mecanisme_sans_inventer_sa_precision() {
        let view = oaat_group_view(json!({"id": "salon"}));

        assert_eq!(view["synchronization"]["supported"], true);
        assert_eq!(view["synchronization"]["transport"], "oaat");
        assert_eq!(
            view["synchronization"]["mechanism"],
            "clock_sync_and_presentation_timestamps"
        );
        assert!(view["synchronization"]["accuracy_claim_ms"].is_null());
    }

    #[test]
    fn aucune_route_ne_rebaptise_un_demi_rtt_en_latence_audio() {
        let manager = include_str!("zone_manager.rs");
        let zones = include_str!("zones.rs");

        let demi_rtt = concat!("rtt_ms", " / 2");
        let fausse_estimation = concat!("estimated_", "latency_ms");
        assert!(!manager.contains(demi_rtt));
        assert!(!zones.contains(demi_rtt));
        assert!(!manager.contains(fausse_estimation));
    }

    #[test]
    fn la_documentation_ne_promet_plus_ntp_ni_sub_milliseconde_hors_preuve() {
        let guide_fr = include_str!("../../../docs/getting-started/fr.md");
        let guide_en = include_str!("../../../docs/getting-started/en.md");
        let architecture = include_str!("../../../docs/architecture-tune-server-rust.md");

        assert!(!guide_fr.contains("synchronise les sorties via NTP"));
        assert!(!guide_en.contains("synchronizes outputs via NTP"));
        assert!(!architecture.contains("Synchronisation sub-milliseconde"));
        assert!(guide_fr.contains("Seuls les points de diffusion Tune"));
        assert!(architecture.contains("Hors OAAT"));
    }
}
