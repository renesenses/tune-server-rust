use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::room_correction::{
    CorrectionFilter, FilterType, FrequencyPoint, RoomProfile, delete_profile,
    generate_correction_from_measurements, list_profiles, load_profile, save_profile,
};

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/profiles", get(list_profiles_handler))
        .route(
            "/profiles/{zone_id}",
            get(get_profile_handler)
                .post(save_profile_handler)
                .delete(delete_profile_handler),
        )
        .route("/analyze", post(analyze_handler))
        .route("/profiles/{zone_id}/apply", post(apply_profile_handler))
        .route("/ir/upload/{zone_id}", post(upload_ir_handler))
        .route("/ir/clear/{zone_id}", post(clear_ir_handler))
        .route("/ir/status/{zone_id}", get(ir_status_handler))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /room-correction/profiles` — list all room correction profiles.
async fn list_profiles_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let profiles = list_profiles(&state.backend);
    Ok(Json(json!({
        "profiles": profiles,
        "count": profiles.len(),
    }))
    .into_response())
}

/// `GET /room-correction/profiles/{zone_id}` — get a zone's profile.
async fn get_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    match load_profile(&state.backend, &zone_id) {
        Some(profile) => Ok(Json(json!(profile)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no profile for this zone"})),
        )
            .into_response()),
    }
}

/// Request body for saving a room correction profile.
#[derive(Deserialize)]
struct SaveProfileBody {
    name: String,
    #[serde(default)]
    filters: Vec<CorrectionFilterInput>,
    /// Raw measurement data (JSON-encoded) for storage / re-analysis.
    measurement_data: Option<String>,
}

#[derive(Deserialize)]
struct CorrectionFilterInput {
    frequency_hz: f64,
    gain_db: f64,
    #[serde(default = "default_q")]
    q_factor: f64,
    #[serde(default = "default_filter_type")]
    filter_type: FilterType,
}

fn default_q() -> f64 {
    1.0
}

fn default_filter_type() -> FilterType {
    FilterType::Peaking
}

/// `POST /room-correction/profiles/{zone_id}` — save a profile.
async fn save_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
    Json(body): Json<SaveProfileBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let filters: Vec<CorrectionFilter> = body
        .filters
        .into_iter()
        .map(|f| CorrectionFilter {
            frequency_hz: f.frequency_hz,
            gain_db: f.gain_db,
            q_factor: f.q_factor,
            filter_type: f.filter_type,
        })
        .collect();

    let now = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Simple ISO-8601 from epoch — good enough for a creation timestamp.
        let dt = time::OffsetDateTime::from_unix_timestamp(secs as i64)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        dt.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| format!("{secs}"))
    };
    let profile = RoomProfile {
        name: body.name,
        zone_id: zone_id.clone(),
        filters,
        created_at: now,
        measurement_data: body.measurement_data,
    };

    save_profile(&state.backend, &profile).map_err(AppError::internal)?;

    Ok((StatusCode::CREATED, Json(json!(profile))).into_response())
}

/// `DELETE /room-correction/profiles/{zone_id}` — delete a zone's profile.
async fn delete_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let existed = delete_profile(&state.backend, &zone_id).map_err(AppError::internal)?;
    if existed {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no profile for this zone"})),
        )
            .into_response())
    }
}

/// Request body for the analyze endpoint.
#[derive(Deserialize)]
struct AnalyzeBody {
    measurements: Vec<FrequencyPoint>,
}

/// `POST /room-correction/analyze` — analyze measurement data and return
/// suggested correction filters without saving anything.
async fn analyze_handler(
    State(state): State<AppState>,
    Json(body): Json<AnalyzeBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    if body.measurements.is_empty() {
        return Err(AppError::bad_request("measurements array is empty"));
    }

    let filters = generate_correction_from_measurements(&body.measurements);

    Ok(Json(json!({
        "filters": filters,
        "filter_count": filters.len(),
        "measurement_points": body.measurements.len(),
    }))
    .into_response())
}

/// `POST /room-correction/profiles/{zone_id}/apply` — apply a zone's room
/// correction profile to the zone's EQ (writes to the existing parametric
/// EQ settings key so the playback pipeline picks it up).
async fn apply_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let profile = match load_profile(&state.backend, &zone_id) {
        Some(p) => p,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "no profile for this zone"})),
            )
                .into_response());
        }
    };

    if profile.filters.is_empty() {
        return Ok(Json(json!({
            "applied": false,
            "zone_id": zone_id,
            "reason": "profile has no correction filters",
        }))
        .into_response());
    }

    // Convert correction filters to the EQ band format used by the existing
    // parametric EQ system (eq_pro / zone DSP).
    let bands: Vec<Value> = profile
        .filters
        .iter()
        .map(|f| {
            json!({
                "freq": f.frequency_hz,
                "gain": f.gain_db,
                "q": f.q_factor,
                "type": match f.filter_type {
                    FilterType::Peaking => "peak",
                    FilterType::LowShelf => "low_shelf",
                    FilterType::HighShelf => "high_shelf",
                    FilterType::Notch => "notch",
                },
            })
        })
        .collect();

    // Write to the zone's EQ profile key (same key the playback DSP reads).
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let eq_profile = json!({
        "enabled": true,
        "source": "room_correction",
        "profile_name": profile.name,
        "bands": bands,
        "preamp_db": 0.0,
    });

    settings
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&eq_profile).map_err(|e| AppError::internal(e.to_string()))?,
        )
        .map_err(AppError::internal)?;

    // Persister ne suffit pas : sans ceci la correction n'atteint le son qu'a
    // la piste SUIVANTE sur une zone locale, alors que la reponse annonce deja
    // `applied: true` (#1725). Une correction de piece se juge a l'oreille,
    // musique en cours — c'est le geste meme qu'on attend de l'utilisateur.
    // `zone_id` est textuel sur cette route ; l'orchestrateur indexe par i64.
    let applique_a_chaud = match zone_id.parse::<i64>() {
        Ok(id) => state.orchestrator.apply_eq_change(id).await,
        Err(_) => false,
    };

    Ok(Json(json!({
        "applied": true,
        "zone_id": zone_id,
        "profile_name": profile.name,
        "filter_count": profile.filters.len(),
        // `applied` dit « persiste » ; celui-ci dit « entendu maintenant ».
        // Faux ne signale pas un echec : rien ne joue, zone non locale, PURE.
        "applied_live": applique_a_chaud,
    }))
    .into_response())
}

/// Le canal vise par un televersement de filtre.
#[derive(Deserialize, Default)]
struct CanalDuFiltre {
    /// `left` ou `right` pour deposer UN canal ; absent = le fichier porte deja
    /// la correction complete (mono duplique, ou stereo tel quel).
    channel: Option<String>,
}

/// `POST /room-correction/ir/upload/{zone_id}` — upload a WAV impulse response
///
/// `?channel=left` / `?channel=right` depose UN canal a la fois. Les outils de
/// correction de piece — REW, Acourate, Audiolense — exportent deux fichiers
/// mono, `filter_L.wav` et `filter_R.wav`, jamais un stereo : sans ce chemin,
/// l'utilisateur devait les fusionner lui-meme dans un editeur audio (Daniel,
/// 24/08/2026).
///
/// Les deux canaux deposes sont combines en un WAV stereo ecrit au chemin que
/// les consommateurs lisent DEJA — sortie locale, transcodage vers les
/// renderers reseau, visualisation `/convolver/response`. Aucun d'eux n'a a
/// connaitre ce nouveau mode.
///
/// Tant qu'il manque un canal, la correction n'est PAS activee : n'appliquer
/// qu'une oreille serait pire que ne rien appliquer. La reponse dit lequel
/// manque.
async fn upload_ir_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(canal): Query<CanalDuFiltre>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    };

    let device_id = zone.output_device_id.unwrap_or_default();
    let is_local = device_id.starts_with("local:");

    // Persist the IR to disk.
    let ir_dir =
        std::path::PathBuf::from(std::env::var("TUNE_DATA_DIR").unwrap_or_else(|_| ".".into()))
            .join("ir");
    std::fs::create_dir_all(&ir_dir).ok();
    let ir_path = ir_dir.join(format!("zone_{zone_id}.wav"));
    let cote = match canal.channel.as_deref() {
        None | Some("") => None,
        Some("left") | Some("l") | Some("gauche") => Some("L"),
        Some("right") | Some("r") | Some("droite") => Some("R"),
        Some(autre) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("canal inconnu « {autre} » — attendu left ou right")
                })),
            )
                .into_response();
        }
    };

    // Depot d'un seul canal : on garde le fichier brut de son cote, et on ne
    // combine que lorsque les DEUX sont la.
    if let Some(cote) = cote {
        let chemin_cote = ir_dir.join(format!("zone_{zone_id}_{cote}.wav"));
        if let Err(e) = std::fs::write(&chemin_cote, &body) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("write IR: {e}")})),
            )
                .into_response();
        }
        let gauche = ir_dir.join(format!("zone_{zone_id}_L.wav"));
        let droite = ir_dir.join(format!("zone_{zone_id}_R.wav"));
        if !gauche.exists() || !droite.exists() {
            let manquant = if gauche.exists() { "right" } else { "left" };
            return Json(json!({
                "ok": true,
                "zone_id": zone_id,
                "channel": if cote == "L" { "left" } else { "right" },
                "awaiting_channel": manquant,
                "active": false,
                "size_bytes": body.len(),
                "message": format!(
                    "filtre {} enregistre — la correction s'activera au depot du canal {manquant}",
                    if cote == "L" { "gauche" } else { "droit" }
                ),
            }))
            .into_response();
        }
        if let Err(e) = tune_core::audio::convolver::Convolver::combiner_en_stereo(
            gauche.to_str().unwrap_or(""),
            droite.to_str().unwrap_or(""),
            ir_path.to_str().unwrap_or(""),
        ) {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    } else if let Err(e) = std::fs::write(&ir_path, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write IR: {e}")})),
        )
            .into_response();
    }
    let ir_path_str = ir_path.to_str().unwrap_or("").to_string();

    // Store the path so the TRANSCODE path picks it up too: the orchestrator's
    // convolver applies the FIR to the bytes served to a network renderer
    // (DLNA/UPnP/AirPlay). Room correction is no longer local-output only.
    SettingsRepo::with_backend(state.backend.clone())
        .set(&format!("ir_path_{zone_id}"), &ir_path_str)
        .ok();

    // A LOCAL output additionally gets the convolver installed immediately (it
    // runs its own real-time convolver rather than going through the transcode).
    #[cfg(feature = "local-audio")]
    {
        if is_local {
            let outputs = state.outputs.lock().await;
            if let Some(output) = outputs.get(&device_id) {
                let output = output.lock().await;
                if let Some(local) = output
                    .as_any()
                    .downcast_ref::<tune_core::outputs::local::LocalOutput>()
                {
                    if let Err(e) = local.set_convolver_ir(&ir_path_str) {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": e})))
                            .into_response();
                    }
                }
            }
        }
    }

    Json(json!({
        "ok": true,
        "zone_id": zone_id,
        "ir_path": ir_path_str,
        "size_bytes": body.len(),
        "active": true,
        "per_channel": cote.is_some(),
        "applies_to": if is_local { "local" } else { "network (transcode)" },
    }))
    .into_response()
}

/// `POST /room-correction/ir/clear/{zone_id}` — remove FIR convolution
async fn clear_ir_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    };

    let device_id = zone.output_device_id.unwrap_or_default();

    // Drop the stored path (network/transcode) + the file for any zone.
    SettingsRepo::with_backend(state.backend.clone())
        .delete(&format!("ir_path_{zone_id}"))
        .ok();
    let ir_path =
        std::path::PathBuf::from(std::env::var("TUNE_DATA_DIR").unwrap_or_else(|_| ".".into()))
            .join("ir")
            .join(format!("zone_{zone_id}.wav"));
    std::fs::remove_file(&ir_path).ok();
    // Et les deux depots par canal, sinon un « effacer » suivi d'un depot d'un
    // seul cote ressusciterait l'ancien filtre de l'autre.
    for cote in ["L", "R"] {
        let mut p = ir_path.clone();
        p.set_file_name(format!("zone_{zone_id}_{cote}.wav"));
        std::fs::remove_file(&p).ok();
    }

    // A local output also drops its live convolver.
    #[cfg(feature = "local-audio")]
    {
        if device_id.starts_with("local:") {
            let outputs = state.outputs.lock().await;
            if let Some(output) = outputs.get(&device_id) {
                let output = output.lock().await;
                if let Some(local) = output
                    .as_any()
                    .downcast_ref::<tune_core::outputs::local::LocalOutput>()
                {
                    local.clear_convolver();
                }
            }
        }
    }
    let _ = &device_id;
    Json(json!({"ok": true, "zone_id": zone_id})).into_response()
}

/// `GET /room-correction/ir/status/{zone_id}` — check if FIR is active
async fn ir_status_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => return Json(json!({"active": false, "zone_id": zone_id})).into_response(),
    };

    let device_id = zone.output_device_id.unwrap_or_default();
    let ir_path = SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("ir_path_{zone_id}"))
        .ok()
        .flatten();
    let has_setting = ir_path.as_deref().map(|p| !p.is_empty()).unwrap_or(false);

    // Local outputs run a live convolver; a network zone applies the IR on the
    // transcode path, so the stored path is the source of truth there.
    #[cfg(feature = "local-audio")]
    {
        if device_id.starts_with("local:") {
            let outputs = state.outputs.lock().await;
            if let Some(output) = outputs.get(&device_id) {
                let output = output.lock().await;
                if let Some(local) = output
                    .as_any()
                    .downcast_ref::<tune_core::outputs::local::LocalOutput>()
                {
                    return Json(json!({
                        "active": local.has_convolver() || has_setting,
                        "zone_id": zone_id,
                        "ir_path": ir_path,
                    }))
                    .into_response();
                }
            }
        }
    }
    let _ = &device_id;
    Json(json!({"active": has_setting, "zone_id": zone_id, "ir_path": ir_path})).into_response()
}
