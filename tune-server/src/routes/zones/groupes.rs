use super::*;

#[derive(Deserialize)]
pub(super) struct CreateGroup {
    /// Optional. The web client groups a *selection* of zones and has no name
    /// to offer, so it never sent this field; when it was mandatory serde
    /// rejected the whole body and axum answered a bare `422 Unprocessable
    /// Entity` with no text at all — the unexplained code the tester saw
    /// (#1702). Absent or blank, the group is named after its zones.
    #[serde(default)]
    pub(super) name: Option<String>,
    pub(super) zone_ids: Vec<i64>,
    /// The zone the others follow. Sent by the web client; defaults to the
    /// first zone of the selection.
    #[serde(default)]
    pub(super) leader_id: Option<i64>,
}

/// Why a set of zones cannot form a group.
///
/// Kept separate from the HTTP layer so the rules can be unit-tested without a
/// database, and so every refusal is forced to carry the words explaining it.
#[derive(Debug, PartialEq)]
pub(super) enum GroupRefusal {
    /// Fewer than two *distinct* zones.
    NotEnoughZones,
    /// A zone id that no longer exists (stale list on the client's side).
    UnknownZone(i64),
    /// Two zones bound to the same output. Tune creates one zone per
    /// discovered output and duplicates do happen ("PC" and "Haut parleurs"
    /// on one sound card, #1702): grouping them would send the same stream
    /// twice to a single device. Both names travel with the refusal so the
    /// message can say *which* zones clash.
    SameOutput(String, String),
}

/// Check a grouping request against the zones that actually exist.
///
/// Returns the de-duplicated zone ids, in request order, when the group is
/// legitimate.
pub(super) fn validate_group(zone_ids: &[i64], zones: &[Zone]) -> Result<Vec<i64>, GroupRefusal> {
    let mut unique: Vec<i64> = Vec::new();
    for &id in zone_ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    if unique.len() < 2 {
        return Err(GroupRefusal::NotEnoughZones);
    }

    // (output device, name of the zone that claimed it first)
    let mut claimed: Vec<(&str, &str)> = Vec::new();
    for &id in &unique {
        let zone = zones
            .iter()
            .find(|z| z.id == Some(id))
            .ok_or(GroupRefusal::UnknownZone(id))?;
        // A zone with no output device is an orphan, not a duplicate: several
        // of them share "nothing", which is not the same output.
        let Some(device) = zone.output_device_id.as_deref().filter(|d| !d.is_empty()) else {
            continue;
        };
        if let Some((_, first)) = claimed.iter().find(|(d, _)| *d == device) {
            return Err(GroupRefusal::SameOutput(
                (*first).to_string(),
                zone.name.clone(),
            ));
        }
        claimed.push((device, zone.name.as_str()));
    }
    Ok(unique)
}

/// Turn a refusal into the error envelope the web client understands:
/// `error` is the machine code, `message` the sentence it displays.
pub(super) fn group_refusal_response(
    refusal: &GroupRefusal,
    lang: &str,
) -> axum::response::Response {
    let (status, code, message) = match refusal {
        GroupRefusal::NotEnoughZones => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "group_needs_two_zones",
            crate::i18n::t(lang, "zonegroup.needsTwoZones"),
        ),
        GroupRefusal::UnknownZone(id) => (
            StatusCode::NOT_FOUND,
            "group_unknown_zone",
            crate::i18n::t(lang, "zonegroup.unknownZone").replace("{id}", &id.to_string()),
        ),
        GroupRefusal::SameOutput(a, b) => (
            StatusCode::CONFLICT,
            "group_same_output",
            crate::i18n::t(lang, "zonegroup.sameOutput")
                .replace("{a}", a)
                .replace("{b}", b),
        ),
    };
    warn!(code, message = %message, "zone_group_refused");
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

pub(super) async fn list_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Groups stored before #1702 have no `group_id`. The web client keys its
    // "dissolve" button on that field and was putting `undefined` in the URL,
    // so an existing group could not be undone. Derive it from `id` on read.
    for group in &mut groups {
        if group.get("group_id").is_none()
            && let Some(id) = group.get("id").and_then(|v| v.as_i64())
        {
            group["group_id"] = json!(id.to_string());
        }
    }
    Json(json!(groups))
}

pub(super) async fn create_group(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateGroup>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: Multiroom sync requires Premium
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::MultiroomSync,
    )
    .await
    {
        return Ok(resp);
    }

    let lang = crate::i18n::lang_from_header(&headers);
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default();
    let zone_ids = match validate_group(&body.zone_ids, &zones) {
        Ok(ids) => ids,
        Err(refusal) => return Ok(group_refusal_response(&refusal, &lang)),
    };

    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            zone_ids
                .iter()
                .filter_map(|id| zones.iter().find(|z| z.id == Some(*id)))
                .map(|z| z.name.as_str())
                .collect::<Vec<_>>()
                .join(" + ")
        });
    let leader_id = body
        .leader_id
        .filter(|id| zone_ids.contains(id))
        .unwrap_or(zone_ids[0]);

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = groups.len() as i64 + 1;
    let group = json!({
        "id": id,
        "group_id": id.to_string(),
        "name": name,
        "leader_id": leader_id,
        "zone_ids": zone_ids,
    });
    groups.push(group.clone());

    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupCreated,
        json!({ "id": id, "name": name, "zone_ids": zone_ids }),
    );
    Ok((StatusCode::CREATED, Json(group)).into_response())
}

#[derive(Deserialize)]
pub(super) struct PatchGroup {
    pub(super) name: Option<String>,
    pub(super) zone_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
pub(super) struct GroupVolumeRequest {
    pub(super) master_volume: Option<f64>,
    pub(super) offsets: Option<std::collections::HashMap<String, f64>>,
}

pub(super) async fn patch_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Json(body): Json<PatchGroup>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match idx {
        Some(i) => {
            if let Some(ref name) = body.name {
                groups[i]["name"] = json!(name);
            }
            if let Some(ref zone_ids) = body.zone_ids {
                groups[i]["zone_ids"] = json!(zone_ids);
            }
            let result = groups[i].clone();
            settings
                .set("zone_groups", &serde_json::to_string(&groups)?)
                .ok();
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::GroupUpdated,
                json!({ "id": group_id, "group": result }),
            );
            Ok(Json(result).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub(super) async fn group_volume(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Json(body): Json<GroupVolumeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
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
            settings
                .set("zone_groups", &serde_json::to_string(&groups)?)
                .ok();

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
                    return Ok(crate::routes::playback::output_command_error_response(
                        error,
                    ));
                }
            }
            Ok(Json(json!({"group_id": group_id, "master_volume": master})).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub(super) async fn calibrate_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(_) => crate::routes::zone_manager::audio_calibration_unavailable(group_id),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn group_health(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

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

pub(super) async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    groups.retain(|g| g.get("id").and_then(|v| v.as_i64()) != Some(group_id));
    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupDeleted,
        json!({ "id": group_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn list_stereo_pairs(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(pairs))
}

#[derive(Deserialize)]
pub(super) struct CreateStereoPair {
    pub(super) name: String,
    pub(super) left_device_id: String,
    pub(super) right_device_id: String,
}

pub(super) async fn create_stereo_pair(
    State(state): State<AppState>,
    Json(body): Json<CreateStereoPair>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = pairs.len() as i64 + 1;
    pairs.push(json!({
        "id": id,
        "name": body.name,
        "left_device_id": body.left_device_id,
        "right_device_id": body.right_device_id,
    }));

    settings
        .set("stereo_pairs", &serde_json::to_string(&pairs)?)
        .ok();
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

pub(super) async fn delete_stereo_pair(
    State(state): State<AppState>,
    Path(pair_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    pairs.retain(|p| p.get("id").and_then(|v| v.as_i64()) != Some(pair_id));
    settings
        .set("stereo_pairs", &serde_json::to_string(&pairs)?)
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn list_group_delays(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let raw = settings
        .get("group_delays")
        .unwrap_or(None)
        .unwrap_or_default();
    let delays: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    Json(json!(delays))
}

pub(super) async fn set_group_delay(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut delays: Vec<Value> = settings
        .get("group_delays")
        .unwrap_or(None)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let tech_a = body.get("tech_a").and_then(|v| v.as_str()).unwrap_or("");
    let tech_b = body.get("tech_b").and_then(|v| v.as_str()).unwrap_or("");
    let delay_ms = body.get("delay_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
    delays.retain(|d| {
        !(d.get("tech_a").and_then(|v| v.as_str()) == Some(tech_a)
            && d.get("tech_b").and_then(|v| v.as_str()) == Some(tech_b))
    });
    delays.push(json!({"tech_a": tech_a, "tech_b": tech_b, "delay_ms": delay_ms}));
    settings
        .set(
            "group_delays",
            &serde_json::to_string(&delays).unwrap_or_default(),
        )
        .ok();
    Json(json!(delays))
}
