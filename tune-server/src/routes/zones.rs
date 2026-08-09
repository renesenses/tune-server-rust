use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::audio::formats::AudioFormat;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::{Zone, ZoneRepo};
use tune_core::discovery::xml_parser::fetch_device_description;
use tune_core::outputs::dlna::DlnaOutput;
use tune_core::playback::{PlayState, ZoneState};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateZone {
    name: String,
    output_type: Option<String>,
    output_device_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateVolume {
    /// Accepts both 0.0-1.0 (float from web client) and 0-100 (integer legacy).
    volume: f64,
}

#[derive(Deserialize)]
struct UpdateMuted {
    muted: bool,
}

#[derive(Deserialize)]
struct RenameZone {
    name: String,
}

#[derive(Deserialize)]
struct PatchZone {
    name: Option<String>,
    volume: Option<i32>,
    muted: Option<bool>,
    output_device_id: Option<String>,
    output_type: Option<String>,
    gapless_enabled: Option<bool>,
    sync_delay_ms: Option<i32>,
    /// Max output sample rate in Hz (e.g. 96000, 88200). null = no limit (passthrough).
    max_sample_rate: Option<Option<u32>>,
    /// When enabled, sends audio at 100% volume (bit-perfect) and disables volume sync from device.
    fixed_volume: Option<bool>,
    /// When enabled, automatically generates and queues similar tracks when the queue ends.
    autoplay_enabled: Option<bool>,
    /// DSD output mode: "auto" (probe renderer), "native" (always passthrough), "pcm" (always transcode).
    dsd_mode: Option<String>,
    /// Décalage des paroles synchronisées, en ms (positif = paroles retardées).
    /// Compense la latence entre ce que le serveur sait et ce que l'auditeur
    /// entend — tampon de Tune puis du renderer (#1328).
    lyrics_offset_ms: Option<i32>,
    /// Force native FLAC to a DLNA renderer even if it doesn't advertise FLAC
    /// (empty/failed GetProtocolInfo Sink) — for renderers that decode FLAC but
    /// under-report (Denon Ceol N12).
    dlna_native_flac: Option<bool>,
    /// When enabled, serve ALAC straight to the renderer (bit-perfect, no FLAC
    /// transcode). Only for renderers that decode ALAC natively.
    alac_passthrough: Option<bool>,
    /// When enabled, transcode lossless to WAV/LPCM (not FLAC) for this DLNA
    /// renderer — skips the slow FLAC encoder for hi-res and avoids renderers
    /// whose ALAC decoder pops at start (LHC-56). Overrides alac_passthrough.
    dlna_lpcm: Option<bool>,
    /// When enabled, cap output to 16-bit for this DLNA renderer. For renderers
    /// that advertise `audio/flac` but only decode 16-bit (Ruark R3, #1137):
    /// downconverts hi-res to 16-bit FLAC instead of serving silent 24-bit direct.
    dlna_cap_16bit: Option<bool>,
    /// When enabled, serve genuine 24-bit WAV (instead of the 16-bit LPCM
    /// fallback) to a DLNA renderer that advertises `audio/L24`. The UI only
    /// offers this after the capability probe reports 24-bit LPCM support.
    dlna_wav24: Option<bool>,
    /// Per-zone SetAVTransportURI→Play delay in ms (0 = use config default). Lets
    /// a renderer with a cold-start under-run buffer before its transport clock
    /// starts (first seconds hachées — Cyrille, Yamaha R-N2000A).
    dlna_play_delay_ms: Option<i64>,
    /// Marque choisie par l'utilisateur dans le catalogue (ou « Autre »).
    /// Persistée en setting `zone_{id}_brand`. Chaîne vide = efface l'override.
    brand: Option<String>,
    /// Modèle choisi par l'utilisateur (filtré par marque, ou texte libre).
    /// Persisté en setting `zone_{id}_model`. Chaîne vide = efface l'override.
    model: Option<String>,
}

/// Injecte l'identité appareil d'une zone dans son JSON de sortie :
/// - `brand` / `model` : override choisi par l'utilisateur (peut être `null`) ;
/// - `detected_manufacturer` / `detected_model` : détection UPnP du device
///   assigné (peut être `null`).
///
/// Le client affiche en priorité l'override utilisateur, sinon la détection
/// UPnP (override > détection).
fn inject_device_identity(
    obj: &mut serde_json::Map<String, Value>,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    detected: Option<&tune_core::discovery::device::DiscoveredDevice>,
) {
    let settings = SettingsRepo::with_backend(backend.clone());
    let brand = settings
        .get(&format!("zone_{zone_id}_brand"))
        .ok()
        .flatten();
    let model = settings
        .get(&format!("zone_{zone_id}_model"))
        .ok()
        .flatten();
    obj.insert("brand".into(), json!(brand));
    obj.insert("model".into(), json!(model));
    obj.insert(
        "detected_manufacturer".into(),
        json!(detected.and_then(|d| d.manufacturer.clone())),
    );
    obj.insert(
        "detected_model".into(),
        json!(detected.and_then(|d| d.model.clone())),
    );
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(list_zones).post(create_zone).delete(delete_all_zones),
        )
        .route("/{id}", get(get_zone).patch(patch_zone).delete(delete_zone))
        .route("/{id}/volume", put(update_volume))
        .route("/{id}/muted", put(update_muted))
        .route("/{id}/dsp", get(get_zone_dsp).put(set_zone_dsp))
        .route("/{id}/convolver/response", get(convolver_response))
        .route("/{id}/renderer-capabilities", post(renderer_capabilities))
        .route("/{id}/name", put(rename_zone))
        .route("/sync-status", get(sync_status))
        .route("/{id}/network-health", get(network_health))
        .route("/group-delays", get(list_group_delays).put(set_group_delay))
        .route("/group", get(list_groups).post(create_group))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/list", get(list_groups))
        .route(
            "/group/{group_id}",
            axum::routing::patch(patch_group).delete(delete_group),
        )
        .route(
            "/groups/{group_id}",
            axum::routing::patch(patch_group).delete(delete_group),
        )
        .route(
            "/groups/{group_id}/volume",
            axum::routing::post(group_volume),
        )
        .route(
            "/groups/{group_id}/calibrate",
            axum::routing::post(calibrate_group),
        )
        .route("/groups/{group_id}/health", get(group_health))
        .route(
            "/stereo-pairs",
            get(list_stereo_pairs).post(create_stereo_pair),
        )
        .route(
            "/stereo-pairs/{pair_id}",
            axum::routing::delete(delete_stereo_pair),
        )
}

pub async fn list_zones_handler(State(state): State<AppState>) -> Json<Value> {
    list_zones(State(state)).await
}

async fn get_zone_dsp(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let eq_key = format!("zone_{id}_eq_profile");
    let eq_profile: Option<tune_core::audio::eq::EqProfile> = settings
        .get(&eq_key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Headphone crossfeed config (local output only). Defaults when unset:
    // disabled, amount 0.30, delay 0.30 ms.
    let crossfeed = read_crossfeed_config(&settings, id);

    match repo.get_dsp_config(id) {
        Ok((preset_id, enabled)) => Json(json!({
            "zone_id": id,
            "dsp_preset_id": preset_id,
            "dsp_enabled": enabled,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "zone_id": id,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
        }))
        .into_response(),
    }
}

/// Cache of computed convolver responses, keyed by zone id. The value pairs
/// the filter fingerprint (path + size + mtime) with the full response body:
/// re-uploading an IR rewrites the file, so the fingerprint changes and the
/// entry is recomputed on the next read — no explicit invalidation hook needed.
static CONVOLVER_RESPONSE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<i64, (String, Value)>>,
> = std::sync::OnceLock::new();

/// `GET /zones/{id}/convolver/response` — frequency response of the zone's FIR
/// convolver, for visualisation. Not premium-gated: applying an IR is, reading
/// the resulting curve is not.
///
/// The running convolver only keeps its IR in FFT-partitioned form, so the taps
/// are re-read from the persisted IR file (`ir_path_{zone_id}` setting — same
/// source of truth as `restore_convolvers` and the transcode path). Multi-
/// channel IRs are summarised by channel 0: averaging L/R taps would let
/// inter-channel phase differences cancel and distort the magnitude curve.
async fn convolver_response(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(_)) => {}
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    }

    let ir_path = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("ir_path_{id}"))
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let Some(ir_path) = ir_path else {
        return Json(json!({"loaded": false})).into_response();
    };
    let Ok(meta) = std::fs::metadata(&ir_path) else {
        // Path persisted but file gone (moved data dir…): nothing to plot.
        return Json(json!({"loaded": false})).into_response();
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fingerprint = format!("{ir_path}|{}|{mtime}", meta.len());

    let cache = CONVOLVER_RESPONSE_CACHE.get_or_init(Default::default);
    if let Some((fp, body)) = cache.lock().expect("convolver cache poisoned").get(&id) {
        if *fp == fingerprint {
            return Json(body.clone()).into_response();
        }
    }

    // ~200 log-spaced points × up to 128k taps of f64 accumulation: fast, but
    // not "handler on the async runtime" fast — compute on the blocking pool.
    let computed = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let (ir, sample_rate) = tune_core::audio::convolver::Convolver::read_ir_taps(&ir_path)?;
        if sample_rate == 0 {
            return Err("IR sample rate is 0".into());
        }
        let taps = &ir[0]; // channel 0 (see handler doc)
        let f_hi = 20_000.0f64.min(sample_rate as f64 * 0.45);
        let freqs = tune_core::audio::convolver::log_freq_grid(200, 20.0, f_hi);
        let points: Vec<Value> =
            tune_core::audio::convolver::fir_frequency_response(taps, sample_rate, &freqs)
                .into_iter()
                .map(|p| {
                    json!({
                        "f": (p.freq_hz * 10.0).round() / 10.0,
                        "db": (p.magnitude_db * 100.0).round() / 100.0,
                        "phase_deg": (p.phase_deg * 100.0).round() / 100.0,
                    })
                })
                .collect();
        let latency_ms = taps.len() as f64 / 2.0 / sample_rate as f64 * 1000.0;
        Ok(json!({
            "loaded": true,
            "taps": taps.len(),
            "sample_rate": sample_rate,
            "latency_ms": (latency_ms * 10.0).round() / 10.0,
            "points": points,
        }))
    })
    .await;

    match computed {
        Ok(Ok(body)) => {
            cache
                .lock()
                .expect("convolver cache poisoned")
                .insert(id, (fingerprint, body.clone()));
            Json(body).into_response()
        }
        Ok(Err(e)) => {
            warn!(zone_id = id, error = %e, "convolver_response_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("read IR: {e}")})),
            )
                .into_response()
        }
        Err(e) => {
            warn!(zone_id = id, error = %e, "convolver_response_join_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "response computation failed"})),
            )
                .into_response()
        }
    }
}

/// Read the `zone_{id}_crossfeed` settings row into a normalised JSON object,
/// falling back to defaults (disabled, amount 0.30, delay 0.30 ms) for any
/// missing/invalid field. Shape: `{ enabled, amount, delay_ms }`.
fn read_crossfeed_config(settings: &tune_core::db::settings_repo::SettingsRepo, id: i64) -> Value {
    let stored: Option<Value> = settings
        .get(&format!("zone_{id}_crossfeed"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    let v = stored.unwrap_or(Value::Null);
    let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false);
    let amount = v.get("amount").and_then(|a| a.as_f64()).unwrap_or(0.30);
    let delay_ms = v.get("delay_ms").and_then(|d| d.as_f64()).unwrap_or(0.30);
    json!({
        "enabled": enabled,
        "amount": amount,
        "delay_ms": delay_ms,
    })
}

async fn set_zone_dsp(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::DspEq)
            .await
    {
        return resp;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    // Handle eq_profile if present
    if let Some(eq_val) = body.get("eq_profile") {
        if let Ok(profile) =
            serde_json::from_value::<tune_core::audio::eq::EqProfile>(eq_val.clone())
        {
            let key = format!("zone_{id}_eq_profile");
            let _ = settings.set(&key, &serde_json::to_string(&profile).unwrap_or_default());
        }
    }

    // Handle crossfeed sub-object if present (local-output headphone effect).
    // Same premium gate (Feature::DspEq) as the EQ path above. Ranges clamped:
    // amount 0..0.5, delay_ms 0..5. Persisted to `zone_{id}_crossfeed`.
    let mut crossfeed_saved: Option<Value> = None;
    if let Some(cf_val) = body.get("crossfeed") {
        let enabled = cf_val
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let amount = cf_val
            .get("amount")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.30)
            .clamp(0.0, 0.5);
        let delay_ms = cf_val
            .get("delay_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.30)
            .clamp(0.0, 5.0);
        let normalised = json!({
            "enabled": enabled,
            "amount": amount,
            "delay_ms": delay_ms,
        });
        let key = format!("zone_{id}_crossfeed");
        let _ = settings.set(
            &key,
            &serde_json::to_string(&normalised).unwrap_or_default(),
        );
        crossfeed_saved = Some(normalised);
    }

    let preset_id = body["dsp_preset_id"].as_i64();
    let enabled = body["dsp_enabled"].as_bool().unwrap_or(false);
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let _ = repo.update_dsp(id, preset_id, enabled);

    Json(json!({
        "zone_id": id,
        "dsp_preset_id": preset_id,
        "dsp_enabled": enabled,
        "eq_profile": body.get("eq_profile"),
        "crossfeed": crossfeed_saved,
    }))
    .into_response()
}

async fn sync_status(State(state): State<AppState>) -> Json<Value> {
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = zone_repo.list().unwrap_or_default();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let metrics = state.poller_metrics.lock().await;

    let mut zone_data = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let poller = metrics.get(&zone_id).cloned().unwrap_or_default();
        let group_id = z.group_id.as_deref();
        zone_data.push(json!({
            "zone_id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "position_ms": ps.position_ms,
            "duration_ms": ps.now_playing.as_ref().map(|np| np.duration_ms).unwrap_or(0),
            "now_playing": ps.now_playing.as_ref().map(|np| json!({
                "title": np.title,
                "artist": np.artist_name,
                "album": np.album_title,
            })),
            "group_id": group_id,
            "poller": poller,
        }));
    }

    Json(json!({
        "zones": zone_data,
        "groups": groups,
        "total_zones": zones.len(),
        "playing_count": zone_data.iter().filter(|z| z["state"] == "playing").count(),
    }))
}

async fn network_health(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let metrics = state.poller_metrics.lock().await;
    let poller = metrics.get(&id).cloned().unwrap_or_default();
    let ps = state.playback.get_state(id).await;

    let stream_bytes: u64 = if let Some(ref np) = ps.now_playing
        && let Some(ref sid) = np.stream_id
    {
        let sessions = state.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(sid.as_str())
            .map(|s| s.bytes_sent.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    } else {
        0
    };

    let uptime_s = state.started_at.elapsed().as_secs();
    let bitrate_kbps = if uptime_s > 0 && stream_bytes > 0 {
        (stream_bytes * 8 / 1000) as f64 / uptime_s as f64
    } else {
        0.0
    };

    Json(json!({
        "zone_id": id,
        "bytes_sent": stream_bytes,
        "bitrate_kbps": (bitrate_kbps * 10.0).round() / 10.0,
        "poll_latency_ms": poller.last_latency_ms,
        "max_latency_ms": poller.max_latency_ms,
        "poll_errors": poller.total_errors,
        "total_polls": poller.total_polls,
    }))
}

pub async fn create_zone_handler(
    state: State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<std::net::SocketAddr>,
    mut body: Json<CreateZone>,
) -> impl IntoResponse {
    // Every web client creates its browser-output zone under the same generic
    // name ("Cet ordinateur"), so several clients show up as indistinguishable
    // duplicates in the zone list (Bertrand). Append the client IP so each
    // machine is identifiable. Guarded to avoid doubling the suffix on retries.
    if body.output_type.as_deref() == Some("browser") {
        let ip = client_addr.ip().to_string();
        if !body.name.contains(&ip) {
            body.name = format!("{} ({ip})", body.name.trim());
        }
    }
    create_zone(state, body).await
}

/// Public wrapper for use from ws.rs snapshot builder.
/// Complète le JSON `current_track` d'une zone avec l'ancrage temporel de
/// métadonnée : `metadata_changed_at` (epoch ms, horloge serveur) et
/// `metadata_age_ms` (âge calculé côté serveur — le client s'ancre dessus
/// sans dépendre de la synchronisation de son horloge). Utilisé par les
/// paroles des pistes radio pour caler les lignes sur le début du morceau
/// détecté dans le flux.
pub(crate) fn inject_metadata_anchor(obj: &mut serde_json::Map<String, Value>, ps: &ZoneState) {
    let (Some(ts), Some(age)) = (ps.metadata_changed_at_ms, ps.metadata_age_ms()) else {
        return;
    };
    if let Some(track) = obj.get_mut("current_track").and_then(|v| v.as_object_mut()) {
        track.insert("metadata_changed_at".into(), json!(ts));
        track.insert("metadata_age_ms".into(), json!(age));
    }
}

pub fn build_signal_path_pub(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
    output_container: Option<&str>,
) -> Option<Value> {
    build_signal_path(
        ps,
        zone,
        backend,
        renderer_label,
        audio_backend,
        output_container,
    )
}

/// Build the `signal_path` object for a zone's current playback.
/// Returns `None` when the zone is not playing.
///
/// `audio_backend` is the active audio backend name ("ASIO", "WASAPI",
/// "CoreAudio", "ALSA") used for local zones' signal path display.
///
/// `output_container` is the REAL container currently served on the wire for
/// this zone's active stream session (`AudioStreamer::stream_output_container`,
/// e.g. "wav" / "flac"), or `None` when there is no live session. It lets the
/// DLNA arm show the negotiated WAV/LPCM fallback (`dlna_needs_wav`, decided
/// async) instead of the statically-guessed FLAC transcode target — Sevy's
/// LHC-52 was served WAV yet the path claimed "ALAC → FLAC".
/// Is a WAV/LPCM wire feed to a DLNA/OpenHome renderer bit-perfect?
///
/// Three cases share the WAV wire: a native WAV source (passthrough), the
/// zone forcing 16-bit LPCM (`dlna_lpcm`), or a FLAC/ALAC source that the
/// orchestrator fell back to WAV for. A **native WAV** source is sent
/// byte-for-byte at any bit depth, so it is always bit-perfect. The FLAC/ALAC→WAV
/// fallback is plain 16-bit LPCM unless `dlna_wav24` preserves the full 24 bits,
/// so it is bit-perfect only when the source already fits 16 bits or the 24-bit
/// override is on.
/// L'égaliseur de cette zone modifie-t-il réellement le signal ?
///
/// Miroir exact de `Orchestrator::load_eq_processor` : mode PURE d'abord — il
/// court-circuite tout traitement, donc un profil enregistré n'y change rien —
/// puis profil activé ET gains audibles. Sans ce miroir, l'indicateur
/// bit-perfect et le chemin audio répondraient à deux questions différentes.
fn zone_eq_alters_signal(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> bool {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    // PURE : le PCM atteint la sortie intact, l'égaliseur n'est jamais construit.
    let pure = settings
        .get(&format!("zone_{zone_id}_audiophile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(false);
    if pure {
        return false;
    }
    let Some(profile) = settings
        .get(&format!("zone_{zone_id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<tune_core::audio::eq::EqProfile>(&s).ok())
    else {
        return false;
    };
    if !profile.enabled {
        return false;
    }
    // 44100/2 n'est qu'une sonde : is_enabled() dépend des gains, pas du débit.
    tune_core::audio::eq::EqProcessor::new(&profile, 44100, 2).is_enabled()
}

fn wav_wire_bit_perfect(
    is_lossless: bool,
    source_is_wav: bool,
    dlna_wav24: bool,
    bit_depth: i32,
) -> bool {
    is_lossless && (source_is_wav || dlna_wav24 || bit_depth <= 16)
}

fn build_signal_path(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
    output_container: Option<&str>,
) -> Option<Value> {
    if ps.state == PlayState::Stopped {
        return None;
    }

    let np = ps.now_playing.as_ref()?;

    // Look up track details for format/sample_rate/bit_depth
    let track = np.track_id.and_then(|tid| {
        TrackRepo::with_backend(backend.clone())
            .get(tid)
            .ok()
            .flatten()
    });

    let fmt_str = np
        .format
        .clone()
        .or_else(|| track.as_ref().and_then(|t| t.format.clone()))
        .unwrap_or_else(|| "flac".into());
    let source_format = AudioFormat::from_extension(&fmt_str);
    let is_dsd = matches!(fmt_str.as_str(), "dsd" | "dsf" | "dff");
    // For DSD files, prefer the track's original sample rate and bit depth
    // from the database (which represent the SOURCE format: e.g. 2822400 Hz
    // / 1-bit for DSD64) over the NowPlaying values, which may contain the
    // TRANSCODED PCM values (e.g. 176400 Hz / 24-bit) when the file was
    // converted for network output (DLNA, OpenHome, etc.).
    let sample_rate = if is_dsd {
        track
            .as_ref()
            .and_then(|t| t.sample_rate)
            .or_else(|| np.sample_rate.map(|v| v as i32))
            .unwrap_or(2_822_400)
    } else {
        np.sample_rate
            .map(|v| v as i32)
            .or_else(|| track.as_ref().and_then(|t| t.sample_rate))
            .unwrap_or(44100)
    };
    let bit_depth = if is_dsd {
        track
            .as_ref()
            .and_then(|t| t.bit_depth)
            .or_else(|| np.bit_depth.map(|v| v as i32))
            .unwrap_or(1)
    } else {
        np.bit_depth
            .map(|v| v as i32)
            .or_else(|| track.as_ref().and_then(|t| t.bit_depth))
            .unwrap_or(16)
    };

    let format_name = if is_dsd {
        match sample_rate {
            r if r >= 22_000_000 => "DSD512",
            r if r >= 11_000_000 => "DSD256",
            r if r >= 5_000_000 => "DSD128",
            _ => "DSD64",
        }
    } else if let Some(f) = source_format.as_ref() {
        f.display_name()
    } else {
        // A UPnP/NAS media-server source reports its codec as a MIME type or DLNA
        // profile (e.g. "audio/mp4", "AAC_ISO_320"), not a file extension, so
        // from_extension() returned None and the signal path showed "Unknown"
        // (Yves: NAS as source). Recognize the codec from the raw string instead.
        let l = fmt_str.to_lowercase();
        let is_m4a = l.contains("mp4") || l.contains("m4a") || l.contains("aac");
        if l.contains("alac") || (is_m4a && bit_depth >= 24) {
            // audio/mp4 (M4A) is ambiguous ALAC vs AAC — same container/MIME. A
            // DIDL res@bitsPerSample >= 24 means lossless ALAC, not lossy AAC
            // (Yves: NAS ALAC read 24-bit by the DartZeel but shown as AAC here).
            "ALAC"
        } else if is_m4a {
            "AAC"
        } else if l.contains("mp3") || l.contains("mpeg") {
            "MP3"
        } else if l.contains("flac") {
            "FLAC"
        } else if l.contains("wav") {
            "WAV"
        } else if l.contains("ogg") || l.contains("vorbis") {
            "OGG"
        } else if l.contains("opus") {
            "OPUS"
        } else {
            "Unknown"
        }
    };
    // For a media-server source (no from_extension AudioFormat) the lossless
    // verdict follows the recognized codec name, so a 24-bit ALAC is no longer
    // shown "Avec perte" (Yves).
    let is_lossless = source_format
        .as_ref()
        .map(|f| f.is_lossless())
        .unwrap_or_else(|| matches!(format_name, "ALAC" | "FLAC" | "WAV"));

    let output_type = zone.output_type.as_deref().unwrap_or("local");

    // Determine if DSP is active.
    //
    // Deux sources, et il faut les DEUX : la colonne dsp_preset_id/dsp_enabled
    // de la zone, et le profil d'égaliseur `zone_{id}_eq_profile`. C'est ce
    // dernier qu'écrit le panneau EQ de « Lecture en cours » et que lit le
    // chemin audio (`Orchestrator::zone_has_active_eq`) — l'indicateur ne le
    // consultait pas.
    //
    // Conséquence : Tune pouvait afficher « Bit-Perfect » alors qu'un
    // égaliseur modifiait réellement le signal. Pour un logiciel dont c'est
    // l'argument central, promettre une pureté qu'on ne tient pas est le pire
    // des deux sens possibles de l'erreur (signalement Bilou).
    let zid = zone.id.unwrap_or(0);
    let dsp_enabled = ZoneRepo::with_backend(backend.clone())
        .get_dsp_config(zid)
        .map(|(preset_id, enabled)| enabled && preset_id.is_some())
        .unwrap_or(false)
        || zone_eq_alters_signal(&backend, zid);

    // Volume at 100% means no software volume adjustment.
    // Fixed-volume zones always output at full volume (bit-perfect).
    let volume_full = zone.fixed_volume || ps.volume >= 1.0 || ps.volume <= 0.0; // 0.0 means no software vol set

    // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC) for network outputs.
    // FLAC, WAV, MP3, AAC are natively supported and pass through without transcoding.
    let is_network_output = matches!(
        output_type,
        "dlna" | "openhome" | "chromecast" | "bluos" | "squeezebox"
    );
    // ALAC native passthrough (opt-in per zone): the orchestrator serves the ALAC
    // file straight to a renderer that decodes it (bit-perfect, no FLAC transcode).
    // Mirror the orchestrator's condition (see orchestrator.rs `alac_passthrough`)
    // so the signal path does not show a phantom ALAC→FLAC transcode step when the
    // wire is really ALAC (forum #1131: DartZeel DAC displays ALAC at the right
    // resolution, yet the signal path claimed an ALAC→FLAC transcode).
    // A zone forced to serve WAV/LPCM (`dlna_lpcm`) always transcodes, so it takes
    // precedence over ALAC passthrough — matching the orchestrator.
    let zone_id = zone.id.unwrap_or(0);
    let dlna_lpcm =
        is_network_output && ZoneRepo::with_backend(backend.clone()).get_dlna_lpcm(zone_id);
    // Zone opt-in 16-bit cap (Ruark R3, #1137): mirrors the orchestrator so the
    // signal path shows a real 16-bit downconvert instead of a phantom
    // bit-perfect passthrough when the source is hi-res.
    let dlna_cap_16bit = is_network_output
        && bit_depth > 16
        && ZoneRepo::with_backend(backend.clone()).get_dlna_cap_16bit(zone_id);
    // Zone opt-in: serve genuine 24-bit WAV (audio/L24) instead of the 16-bit
    // LPCM fallback. Mirrors orchestrator.rs `dlna_wav24` so the signal path
    // shows a lossless 24-bit WAV wire (not a phantom 16-bit truncation).
    let dlna_wav24 = is_network_output
        && bit_depth > 16
        && ZoneRepo::with_backend(backend.clone()).get_dlna_wav24(zone_id);
    let alac_passthrough = source_format == Some(AudioFormat::Alac)
        && is_network_output
        && !dlna_lpcm
        && !dlna_wav24
        && !dlna_cap_16bit
        && ZoneRepo::with_backend(backend.clone()).get_alac_passthrough(zone_id);
    let needs_transcode_for_output = is_network_output
        && !alac_passthrough
        && source_format
            .as_ref()
            .is_some_and(|f| f.needs_transcode_for_dlna());
    // OAAT transcodes everything to WAV except WAV itself
    let is_oaat = output_type == "oaat";
    let oaat_transcodes = is_oaat
        && source_format
            .as_ref()
            .is_some_and(|f| *f != AudioFormat::Wav);

    // The renderer may be served WAV/LPCM even for a FLAC/ALAC source when it
    // does not advertise `audio/flac` (`orchestrator::dlna_needs_wav`, decided
    // by async SOAP negotiation this synchronous builder cannot replay). Trust
    // the live session's real container over the static transcode-target guess
    // so the path shows "ALAC → WAV" instead of a phantom "ALAC → FLAC" (Sevy,
    // LHC-52). Only "wav" changes the verdict; anything else keeps prior logic.
    let wire_wav = output_container.is_some_and(|c| c.eq_ignore_ascii_case("wav"));

    let (transport_bit_perfect, transport_desc, output_format_name) = match output_type {
        "dlna" | "openhome" => {
            if wire_wav || dlna_lpcm || dlna_wav24 {
                // Renderer served WAV/LPCM, not FLAC — the signal path must say
                // so (a renderer showing "WAV/PCM" otherwise contradicted Tune's
                // "→ FLAC" label, LHC). Three causes, same wire: the zone forces
                // 16-bit LPCM (`dlna_lpcm`) or genuine 24-bit WAV (`dlna_wav24`),
                // or the renderer doesn't advertise `audio/flac` and the
                // orchestrator fell back to WAV (`dlna_needs_wav`) — detected here
                // from the live session's real container (`wire_wav`), which the
                // synchronous builder cannot renegotiate. The plain LPCM fallback
                // is 16-bit (audio/L16), bit-perfect only when the lossless source
                // already fits 16 bits (Sevy, #1137). The opt-in `dlna_wav24` path
                // preserves the full 24-bit source, so it stays bit-perfect.
                // A *native* WAV source is served byte-for-byte (WAV never
                // transcodes for DLNA), so it is bit-perfect at any depth
                // regardless of `dlna_wav24` — which only governs the FLAC/ALAC→WAV
                // fallback (Sandro/Progman: WAV 24-bit direct showed red without it).
                let wav_bit_perfect = wav_wire_bit_perfect(
                    is_lossless,
                    matches!(source_format, Some(AudioFormat::Wav)),
                    dlna_wav24,
                    bit_depth,
                );
                (wav_bit_perfect, "DLNA/UPnP", "WAV")
            } else if needs_transcode_for_output || dlna_cap_16bit {
                // Cap forces a 16-bit FLAC downconvert (not bit-perfect) even for
                // an otherwise-direct FLAC source (Ruark R3, #1137).
                let target = source_format
                    .map(|f| f.dlna_transcode_target())
                    .unwrap_or(AudioFormat::Flac);
                (false, "DLNA/UPnP", target.display_name())
            } else {
                // FLAC, WAV, MP3, AAC → passthrough (bit-perfect for lossless)
                (true, "DLNA/UPnP", format_name)
            }
        }
        "oaat" => {
            // Lossless PCM → WAV preserves every bit, but DSD → WAV is a domain
            // conversion (1-bit sigma-delta decimated to multi-bit PCM), so it is
            // NOT bit-perfect even though DSD counts as a lossless *format*.
            (
                (is_lossless && !is_dsd) || !oaat_transcodes,
                "OAAT",
                if oaat_transcodes { "WAV" } else { format_name },
            )
        }
        "airplay" => (false, "AirPlay", "ALAC"),
        "chromecast" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "Chromecast", target.display_name())
            } else {
                (false, "Chromecast", format_name)
            }
        }
        "bluos" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "BluOS", target.display_name())
            } else {
                (true, "BluOS", format_name)
            }
        }
        "squeezebox" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "Squeezebox", target.display_name())
            } else {
                (true, "Squeezebox", format_name)
            }
        }
        "browser" => (true, "Browser", format_name),
        "local" => {
            // Show the actual audio backend (ASIO / WASAPI / CoreAudio / ALSA)
            let transport = match audio_backend {
                "ASIO" => "ASIO (exclusive)",
                "WASAPI" => "WASAPI",
                "CoreAudio" => "CoreAudio",
                "ALSA" => "ALSA",
                other => other,
            };
            (true, transport, format_name)
        }
        other => (false, other, format_name),
    };

    // Detect sample rate capping (DSD excluded — the DSD→PCM transcode
    // already handles rate conversion; showing a separate resampler step
    // would be misleading since sample_rate here is the DSD MHz rate).
    let resampling_active = !is_dsd
        && zone
            .max_sample_rate
            .is_some_and(|max| (sample_rate as u32) > max);

    // Overall bit-perfect: lossless source + no transcoding + no DSP + no resampling.
    // Volume is excluded — it's a user preference, not a signal degradation.
    let bit_perfect = is_lossless && transport_bit_perfect && !dsp_enabled && !resampling_active;

    // Build steps
    let source_desc = if is_dsd {
        // DSD rates are in MHz range — display as e.g. "DSD64 2.8 MHz" or "DSD128 5.6 MHz"
        let mhz = sample_rate as f64 / 1_000_000.0;
        format!("{format_name} {mhz:.1} MHz")
    } else if sample_rate >= 1000 {
        format!(
            "{format_name} {sr}kHz/{bit_depth}bit",
            sr = sample_rate / 1000
        )
    } else {
        format!("{format_name} {sample_rate}Hz/{bit_depth}bit")
    };

    let mut steps = vec![json!({
        "name": "Source",
        "description": source_desc,
        "bit_perfect": true,
    })];

    // Decoder step. Skipped for DSD: the Source already reads e.g.
    // "DSD64 2.8 MHz" and the DSD→PCM/FLAC conversion is shown by the Transcoder
    // step, so a bare "DSD64" decoder line was just a confusing duplicate.
    if !is_dsd {
        steps.push(json!({
            "name": "Decoder",
            "description": format_name,
            "bit_perfect": is_lossless,
        }));
    }

    // Transcoding step (only if transcoding occurs). Include the zone-forced
    // WAV/LPCM (dlna_lpcm), 16-bit-cap (dlna_cap_16bit) and async WAV-fallback
    // (wire_wav) paths: all re-encode the stream, so the step must appear even
    // when the source format itself wouldn't need transcoding for DLNA (a FLAC
    // source with LPCM/cap-16, or an ALAC/FLAC source to a renderer that fell
    // back to WAV) — otherwise the path claimed a bit-perfect passthrough that
    // isn't happening (LHC: renderer shows WAV 16/44 while Tune showed ALAC→FLAC;
    // Sevy: LHC-52 served WAV while Tune showed ALAC→FLAC).
    let wire_transcode = wire_wav && !matches!(format_name, "WAV");
    let transcode_active = needs_transcode_for_output
        || oaat_transcodes
        || output_type == "airplay"
        || dlna_lpcm
        || dlna_wav24
        || dlna_cap_16bit
        || wire_transcode;
    if transcode_active {
        // OAAT lossless PCM → WAV preserves all audio data, but DSD → WAV is a
        // lossy domain conversion (see the "oaat" transport arm above). A DLNA
        // WAV/LPCM output likewise preserves the samples only when the source
        // already fits the 16-bit LPCM cap — unless the zone opted into genuine
        // 24-bit WAV (`dlna_wav24`), which keeps the full depth.
        let wav_output = wire_wav || dlna_lpcm || dlna_wav24;
        let transcode_lossless = (is_oaat && is_lossless && !is_dsd)
            || (wav_output && is_lossless && (dlna_wav24 || bit_depth <= 16));
        // Reflect the OUTPUT resolution the renderer actually receives: 24-bit
        // for the opt-in 24-bit WAV path, 16-bit when the zone caps to 16-bit OR
        // serves the plain LPCM fallback (audio/L16 is 16-bit), and the
        // max-sample-rate cap when set.
        let out_bit_depth = if dlna_wav24 {
            bit_depth.min(24)
        } else if dlna_cap_16bit || wav_output {
            bit_depth.min(16)
        } else {
            bit_depth
        };
        let out_sample_rate = zone
            .max_sample_rate
            .map(|m| (sample_rate as u32).min(m) as i32)
            .unwrap_or(sample_rate);
        let out_desc = if out_sample_rate >= 1000 {
            format!(
                "{output_format_name} {sr}kHz/{out_bit_depth}bit",
                sr = out_sample_rate / 1000
            )
        } else {
            format!("{output_format_name} {out_sample_rate}Hz/{out_bit_depth}bit")
        };
        steps.push(json!({
            "name": "Transcoder",
            "description": format!("{source_desc} \u{2192} {out_desc}"),
            "bit_perfect": transcode_lossless,
        }));
    }

    // Resampler step (when zone max_sample_rate caps the output)
    if resampling_active {
        let max_sr = zone.max_sample_rate.unwrap();
        let src_khz = sample_rate / 1000;
        let dst_khz = max_sr / 1000;
        steps.push(json!({
            "name": "Resampler",
            "description": format!("{src_khz}kHz \u{2192} {dst_khz}kHz"),
            "bit_perfect": false,
        }));
    }

    // Volume step (informational — does not affect bit-perfect status)
    if !volume_full {
        steps.push(json!({
            "name": "Volume",
            "description": format!("Volume {}%", (ps.volume * 100.0).round() as i32),
            "bit_perfect": true,
        }));
    }

    // DSP step
    if dsp_enabled {
        steps.push(json!({
            "name": "DSP",
            "description": "EQ/DSP active",
            "bit_perfect": false,
        }));
    }

    // Transport step
    steps.push(json!({
        "name": "Transport",
        "description": transport_desc,
        "bit_perfect": transport_bit_perfect,
    }));

    let renderer_name = renderer_label
        .or(zone.output_device_id.as_deref())
        .unwrap_or(output_type);
    steps.push(json!({
        "name": "Renderer",
        "description": renderer_name,
        "bit_perfect": transport_bit_perfect,
    }));

    // Build summary
    let bp_label = if bit_perfect { " (bit-perfect)" } else { "" };
    let summary = if transcode_active {
        format!(
            "{format_name} \u{2192} {output_format_name} transcode \u{2192} {transport_desc}{bp_label}"
        )
    } else {
        format!("{format_name} \u{2192} {transport_desc}{bp_label}")
    };

    Some(json!({
        "bit_perfect": bit_perfect,
        // Whether the *source* is a lossless format (FLAC, ALAC, WAV, DSD, …).
        // Distinct from bit_perfect: a lossless source transcoded to another
        // lossless container (DSD→FLAC, ALAC→FLAC for a DLNA renderer) is not
        // bit-perfect but is still lossless — the UI must not call it "lossy".
        "lossless": is_lossless,
        "summary": summary,
        "steps": steps,
    }))
}

async fn list_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = repo.list().unwrap_or_default();
    let devices = state.scanner.lock().await.devices().await;
    // Manually-added devices (e.g. legacy DLNA renderers that never appear in
    // SSDP discovery) are registered as outputs but absent from `devices`.
    // Treat a registered output as online too, otherwise its zone is shown
    // offline even though playback works.
    let registered_output_ids: std::collections::HashSet<String> =
        state.outputs.lock().await.list().into_iter().collect();
    let default_zone_id: Option<i64> =
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get("default_zone_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
    #[cfg(feature = "local-audio")]
    let audio_backend =
        tune_core::outputs::local::active_backend_name(&state.display_audio_backend());
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let mut v = serde_json::to_value(z).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "state".into(),
                json!(match ps.state {
                    tune_core::playback::PlayState::Playing => "playing",
                    tune_core::playback::PlayState::Paused => "paused",
                    tune_core::playback::PlayState::Stopped => "stopped",
                }),
            );
            obj.insert("current_track".into(), json!(ps.now_playing));
            inject_metadata_anchor(obj, &ps);
            obj.insert("position_ms".into(), json!(ps.position_ms));
            obj.insert("queue_length".into(), json!(ps.queue_length));
            obj.insert(
                "volume".into(),
                json!(if ps.volume > 0.0 {
                    ps.volume
                } else {
                    z.volume as f64 / 100.0
                }),
            );
            let renderer_label = z
                .output_device_id
                .as_deref()
                .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
            let output_container = match ps
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_deref())
            {
                Some(sid) => state.streamer.stream_output_container(sid).await,
                None => None,
            };
            let signal_path = build_signal_path(
                &ps,
                z,
                &state.backend,
                renderer_label,
                audio_backend,
                output_container.as_deref(),
            );
            obj.insert("signal_path".into(), json!(signal_path));
            obj.insert("is_default".into(), json!(default_zone_id == Some(zone_id)));
            let zone_repo = ZoneRepo::with_backend(state.backend.clone());
            obj.insert("dsd_mode".into(), json!(zone_repo.get_dsd_mode(zone_id)));
            obj.insert(
                "lyrics_offset_ms".into(),
                json!(zone_repo.get_lyrics_offset_ms(zone_id)),
            );
            obj.insert(
                "dlna_native_flac".into(),
                json!(zone_repo.get_dlna_native_flac(zone_id)),
            );
            obj.insert(
                "alac_passthrough".into(),
                json!(zone_repo.get_alac_passthrough(zone_id)),
            );
            obj.insert("dlna_lpcm".into(), json!(zone_repo.get_dlna_lpcm(zone_id)));
            obj.insert(
                "dlna_cap_16bit".into(),
                json!(zone_repo.get_dlna_cap_16bit(zone_id)),
            );
            obj.insert(
                "dlna_wav24".into(),
                json!(zone_repo.get_dlna_wav24(zone_id)),
            );
            obj.insert(
                "dlna_play_delay_ms".into(),
                json!(zone_repo.get_dlna_play_delay_ms(zone_id)),
            );
            let detected_dev = z
                .output_device_id
                .as_deref()
                .and_then(|did| devices.iter().find(|d| d.id == did));
            inject_device_identity(obj, &state.backend, zone_id, detected_dev);
            let online = match z.output_type.as_deref() {
                // Browser zones have no output device by design (the web
                // client pulls stream_url itself) — always online.
                Some("browser") => true,
                // A local zone is online as long as it still has a device
                // assigned; an orphan row without output_device_id can never
                // play (Yacine, 24/07) and must be reported offline so
                // clients grey it out. Other types already fall through to
                // unwrap_or(false) when output_device_id is NULL.
                Some("local") => z.output_device_id.is_some(),
                _ => z
                    .output_device_id
                    .as_deref()
                    .map(|id| {
                        devices.iter().any(|d| d.id == id) || registered_output_ids.contains(id)
                    })
                    .unwrap_or(false),
            };
            obj.insert("online".into(), json!(online));
            // Include stream_url for browser playback zones so the web client
            // can feed it to an HTML5 <audio> element.
            if let Some(ref np) = ps.now_playing {
                if let Some(ref stream_id) = np.stream_id {
                    let server_ip = state.config.advertised_ip.clone().unwrap_or_else(|| {
                        tune_core::discovery::ssdp::get_local_ip()
                            .map(|ip| ip.to_string())
                            .unwrap_or_else(|| "127.0.0.1".into())
                    });
                    let stream_url = format!(
                        "http://{}:{}/stream/{}.flac",
                        server_ip, state.port, stream_id
                    );
                    obj.insert("stream_url".into(), json!(stream_url));
                }
            }
        }
        result.push(v);
    }
    Json(json!(result))
}

async fn get_zone(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    #[cfg(feature = "local-audio")]
    let audio_backend =
        tune_core::outputs::local::active_backend_name(&state.display_audio_backend());
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    match repo.get(id) {
        Ok(Some(zone)) => {
            let ps = state.playback.get_state(id).await;
            let mut v = serde_json::to_value(&zone).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "state".into(),
                    json!(match ps.state {
                        tune_core::playback::PlayState::Playing => "playing",
                        tune_core::playback::PlayState::Paused => "paused",
                        tune_core::playback::PlayState::Stopped => "stopped",
                    }),
                );
                obj.insert("current_track".into(), json!(ps.now_playing));
                inject_metadata_anchor(obj, &ps);
                obj.insert("position_ms".into(), json!(ps.position_ms));
                obj.insert("queue_length".into(), json!(ps.queue_length));
                // Expose the queue index too so the client can refresh the
                // "now playing" highlight on track change without refetching the
                // whole queue (expensive under a large shuffle queue, #1096).
                obj.insert("queue_position".into(), json!(ps.queue_position));
                obj.insert("volume".into(), json!(zone.volume as f64 / 100.0));
                let devices = state.scanner.lock().await.devices().await;
                let registered_output_ids: std::collections::HashSet<String> =
                    state.outputs.lock().await.list().into_iter().collect();
                let renderer_label = zone
                    .output_device_id
                    .as_deref()
                    .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
                let output_container = match ps
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.as_deref())
                {
                    Some(sid) => state.streamer.stream_output_container(sid).await,
                    None => None,
                };
                let signal_path = build_signal_path(
                    &ps,
                    &zone,
                    &state.backend,
                    renderer_label,
                    audio_backend,
                    output_container.as_deref(),
                );
                obj.insert("signal_path".into(), json!(signal_path));
                obj.insert("dsd_mode".into(), json!(repo.get_dsd_mode(id)));
                obj.insert(
                    "lyrics_offset_ms".into(),
                    json!(repo.get_lyrics_offset_ms(id)),
                );
                obj.insert(
                    "dlna_native_flac".into(),
                    json!(repo.get_dlna_native_flac(id)),
                );
                obj.insert(
                    "alac_passthrough".into(),
                    json!(repo.get_alac_passthrough(id)),
                );
                obj.insert("dlna_lpcm".into(), json!(repo.get_dlna_lpcm(id)));
                obj.insert("dlna_cap_16bit".into(), json!(repo.get_dlna_cap_16bit(id)));
                obj.insert("dlna_wav24".into(), json!(repo.get_dlna_wav24(id)));
                obj.insert(
                    "dlna_play_delay_ms".into(),
                    json!(repo.get_dlna_play_delay_ms(id)),
                );
                let detected_dev = zone
                    .output_device_id
                    .as_deref()
                    .and_then(|did| devices.iter().find(|d| d.id == did));
                inject_device_identity(obj, &state.backend, id, detected_dev);
                let online = match zone.output_type.as_deref() {
                    // Same rules as list_zones: browser zones need no device;
                    // a local zone without output_device_id is an orphan that
                    // can never play → offline.
                    Some("browser") => true,
                    Some("local") => zone.output_device_id.is_some(),
                    _ => zone
                        .output_device_id
                        .as_deref()
                        .map(|did| {
                            devices.iter().any(|d| d.id == did)
                                || registered_output_ids.contains(did)
                        })
                        .unwrap_or(false),
                };
                obj.insert("online".into(), json!(online));
                // Include stream_url for browser playback zones so the web client
                // can feed it to an HTML5 <audio> element.
                if let Some(ref np) = ps.now_playing {
                    if let Some(ref stream_id) = np.stream_id {
                        let server_ip = state.config.advertised_ip.clone().unwrap_or_else(|| {
                            tune_core::discovery::ssdp::get_local_ip()
                                .map(|ip| ip.to_string())
                                .unwrap_or_else(|| "127.0.0.1".into())
                        });
                        let stream_url = format!(
                            "http://{}:{}/stream/{}.flac",
                            server_ip, state.port, stream_id
                        );
                        obj.insert("stream_url".into(), json!(stream_url));
                    }
                }
            }
            Json(v).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn patch_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    if let Some(ref name) = body.name
        && let Err(e) = repo.update_name(id, name)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(vol) = body.volume
        && let Err(e) = repo.update_volume(id, vol)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(muted) = body.muted
        && let Err(e) = repo.update_muted(id, muted)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(ref device_id) = body.output_device_id
        && let Err(e) = repo.update_output_device(id, device_id)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(ref ot) = body.output_type
        && let Err(e) = repo.update_output_type(id, ot)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(gapless) = body.gapless_enabled
        && let Err(e) = repo.update_gapless_enabled(id, gapless)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(ms) = body.sync_delay_ms
        && let Err(e) = repo.update_sync_delay(id, ms)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(rate) = body.max_sample_rate
        && let Err(e) = repo.update_max_sample_rate(id, rate)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(fixed) = body.fixed_volume {
        if let Err(e) = repo.update_fixed_volume(id, fixed) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        // When enabling fixed_volume, pin volume to 100% in DB and in-memory
        if fixed {
            repo.update_volume(id, 100).ok();
            state.playback.set_volume(id, 1.0).await;
        }
    }
    if let Some(autoplay) = body.autoplay_enabled
        && let Err(e) = repo.update_autoplay_enabled(id, autoplay)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Some(ref mode) = body.dsd_mode {
        if let Err(e) = repo.update_dsd_mode(id, mode) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(offset) = body.lyrics_offset_ms {
        // Borne large mais finie : au-dela d'une minute ce n'est plus un
        // reglage de latence, et une valeur folle desynchroniserait tout.
        let clamped = offset.clamp(-60_000, 60_000);
        if let Err(e) = repo.update_lyrics_offset_ms(id, clamped) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(native_flac) = body.dlna_native_flac {
        if let Err(e) = repo.update_dlna_native_flac(id, native_flac) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(passthrough) = body.alac_passthrough {
        if let Err(e) = repo.update_alac_passthrough(id, passthrough) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(lpcm) = body.dlna_lpcm {
        if let Err(e) = repo.update_dlna_lpcm(id, lpcm) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(cap) = body.dlna_cap_16bit {
        if let Err(e) = repo.update_dlna_cap_16bit(id, cap) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(wav24) = body.dlna_wav24 {
        if let Err(e) = repo.update_dlna_wav24(id, wav24) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(delay) = body.dlna_play_delay_ms {
        let delay = delay.max(0) as u64;
        if let Err(e) = repo.update_dlna_play_delay_ms(id, delay) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        // Apply live to the already-registered output so the new delay takes
        // effect on the next play without a rebuild/restart. 0 = fall back to the
        // config default (`[device_delays]` / `dlna_play_delay_ms`) by name.
        if let Some(device_id) = repo.get(id).ok().flatten().and_then(|z| z.output_device_id) {
            let output = { state.outputs.lock().await.get(&device_id) };
            if let Some(output) = output {
                let guard = output.lock().await;
                // `name()` is an OutputTarget trait method → read it on the trait
                // object before downcasting to the concrete DlnaOutput.
                let effective = if delay > 0 {
                    delay
                } else {
                    state.config.play_delay_for(guard.name())
                };
                if let Some(dlna) = guard.as_any().downcast_ref::<DlnaOutput>() {
                    dlna.set_play_delay(effective);
                }
            }
        }
    }
    // Marque / modèle choisis par l'utilisateur → settings zone_{id}_brand/model.
    // Chaîne vide = suppression de l'override (retour à la détection UPnP).
    if let Some(ref brand) = body.brand {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_brand");
        let r = if brand.trim().is_empty() {
            settings.delete(&key)
        } else {
            settings.set(&key, brand.trim())
        };
        if let Err(e) = r {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Some(ref model) = body.model {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_model");
        let r = if model.trim().is_empty() {
            settings.delete(&key)
        } else {
            settings.set(&key, model.trim())
        };
        if let Err(e) = r {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    get_zone(State(state), Path(id)).await.into_response()
}

async fn create_zone(
    State(state): State<AppState>,
    Json(body): Json<CreateZone>,
) -> impl IntoResponse {
    let output_type = body.output_type.as_deref();
    let output_device_id = body.output_device_id.as_deref();

    // If device already has a zone (visible OR hidden), return it (no premium check needed).
    // A previously soft-deleted zone (is_hidden=1) is resurrected so the user's
    // prior settings (volume, DSP, gapless, etc.) are preserved.
    if let Some(device_id) = output_device_id {
        let repo = ZoneRepo::with_backend(state.backend.clone());
        if let Ok(Some(existing)) = repo.get_by_device_id(device_id) {
            if let Some(id) = existing.id {
                // Unhide if the zone was soft-deleted
                if repo.is_device_hidden(device_id) {
                    info!(
                        zone_id = id,
                        device_id, "unhiding_previously_deleted_zone_via_api"
                    );
                    let _ = repo.unhide(id);
                    // Update name in case device was renamed
                    let _ = repo.update_name(id, &body.name);
                    if let Some(ref ot) = body.output_type {
                        let _ = repo.update_output_type(id, ot);
                    }
                }
                let _ = repo.update_online(id, true);
                let zone = repo.get(id).ok().flatten();
                let v = zone
                    .as_ref()
                    .map(|z| serde_json::to_value(z).unwrap_or_default())
                    .unwrap_or(json!({"id": id}));
                info!(zone_id = id, device_id, "zone_already_exists_returning");
                return (StatusCode::OK, Json(v)).into_response();
            }
        }
    }

    // The free-tier zone cap is enforced at *activation* (first play) in
    // orchestrator.play(), not at creation: creating/discovering a zone is
    // always allowed and the zone starts dormant. This avoids blocking a free
    // user from creating their actual renderer just because auto-discovered
    // zones filled the old count. See PlaybackOrchestrator::enforce_zone_cap.

    // For DLNA/OpenHome zones, ensure the output is registered before persisting
    if let Some(device_id) = output_device_id {
        let is_dlna = matches!(output_type, Some("dlna") | Some("openhome"));
        if is_dlna {
            let already_registered = {
                let outputs = state.outputs.lock().await;
                outputs.get(device_id).is_some()
            };
            if !already_registered {
                // Look up the discovered device and register its DLNA output
                let scanner = state.scanner.lock().await;
                let devices = scanner.devices().await;
                drop(scanner);

                let disc = devices.iter().find(|d| d.id == device_id);
                if let Some(dev) = disc {
                    let registered = register_dlna_output_from_device(dev, &state).await;
                    if !registered {
                        warn!(device_id, "create_zone_output_registration_failed");
                    }
                } else {
                    warn!(device_id, "create_zone_device_not_discovered");
                }
            }
        }

        // For local audio zones, verify the device exists in the OutputRegistry
        if matches!(output_type, Some("local")) && device_id.starts_with("local:") {
            let found = {
                let outputs = state.outputs.lock().await;
                outputs.get(device_id).is_some()
            };
            if !found {
                warn!(device_id, "create_zone_local_device_not_found");
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"detail": format!("Local audio device not found: {device_id}. Make sure the device is connected and detected.")})),
                )
                    .into_response();
            }
        }
    }

    // Duplicate device assignment already handled above (early return)

    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.create(&body.name, output_type, output_device_id) {
        Ok(id) => {
            info!(zone_id = id, name = %body.name, output_type = ?output_type, "zone_created");

            // Build the full zone object for both HTTP response and WS event
            let zone = repo.get(id).ok().flatten();
            let mut v = zone
                .as_ref()
                .and_then(|z| serde_json::to_value(z).ok())
                .unwrap_or_else(|| json!({"id": id, "name": body.name}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("state".into(), json!("stopped"));
                obj.insert("current_track".into(), json!(null));
                obj.insert("position_ms".into(), json!(0));
                obj.insert("queue_length".into(), json!(0));
                let vol = zone.as_ref().map(|z| z.volume).unwrap_or(50);
                obj.insert("volume".into(), json!(vol as f64 / 100.0));
            }

            // Emit with full zone data so clients can merge without re-fetching
            state.event_bus.emit(
                "zone.created",
                json!({
                    "id": id,
                    "zone": &v,
                }),
            );

            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) if e.contains("UNIQUE constraint failed") => {
            // Safety net: a hidden zone with this device_id blocked the INSERT.
            // Unhide it and return it instead of erroring.
            if let Some(device_id) = output_device_id {
                if let Ok(Some(existing)) = repo.get_by_device_id(device_id) {
                    if let Some(id) = existing.id {
                        warn!(
                            zone_id = id,
                            device_id, "unique_constraint_recovery_unhiding_zone"
                        );
                        let _ = repo.unhide(id);
                        let _ = repo.update_name(id, &body.name);
                        let _ = repo.update_online(id, true);
                        let zone = repo.get(id).ok().flatten();
                        let v = zone
                            .as_ref()
                            .map(|z| serde_json::to_value(z).unwrap_or_default())
                            .unwrap_or(json!({"id": id}));
                        return (StatusCode::OK, Json(v)).into_response();
                    }
                }
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": e})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": e})),
        )
            .into_response(),
    }
}

/// POST /zones/{id}/renderer-capabilities — on-demand "discovery check" for the
/// renderer-config UI. Probes the zone's DLNA renderer via GetProtocolInfo and
/// returns which audio formats its `Sink` advertises (FLAC, WAV/LPCM 16 & 24,
/// ALAC/AAC, MP3, DSD), so the user can pick a sensible output override with
/// evidence. Only meaningful for dlna/openhome zones with a live renderer.
async fn renderer_capabilities(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone = match repo.get(id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "zone_not_found" })),
            )
                .into_response();
        }
    };

    if !matches!(zone.output_type.as_deref(), Some("dlna") | Some("openhome")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "not_a_dlna_renderer",
                "message": "Renderer capability discovery is only available for DLNA/OpenHome zones.",
            })),
        )
            .into_response();
    }

    let Some(device_id) = zone.output_device_id.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no_output_device" })),
        )
            .into_response();
    };

    // The GetProtocolInfo probe needs the registered DlnaOutput (it holds the
    // ConnectionManager URL). If the renderer hasn't been played yet it may not
    // be registered — try to register it from the discovered device first, same
    // as create_zone does, so the check works without playing a track first.
    let mut output = { state.outputs.lock().await.get(device_id) };
    if output.is_none() {
        let disc = {
            let scanner = state.scanner.lock().await;
            let devices = scanner.devices().await;
            devices.iter().find(|d| d.id == device_id).cloned()
        };
        if let Some(dev) = disc {
            register_dlna_output_from_device(&dev, &state).await;
            output = { state.outputs.lock().await.get(device_id) };
        }
    }

    let Some(output) = output else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "probed": false,
                "reason": "renderer_offline",
                "message": "The renderer is not currently online/discovered. Make sure it is powered on and on the same network, then try again.",
            })),
        )
            .into_response();
    };

    // Hold the output lock for the SOAP round-trip (on-demand, user-initiated,
    // rare) — same pattern the orchestrator uses for its per-track probe.
    let caps = {
        let guard = output.lock().await;
        match guard.as_any().downcast_ref::<DlnaOutput>() {
            Some(dlna) => dlna.probe_capabilities().await,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "not_a_dlna_output" })),
                )
                    .into_response();
            }
        }
    };

    Json(json!(caps)).into_response()
}

/// Register a DLNA output from a discovered device.
/// Fetches the device description XML to find AVTransport/RenderingControl URLs,
/// then registers the output in the global registry.
/// Returns true if registration succeeded.
async fn register_dlna_output_from_device(
    dev: &tune_core::discovery::device::DiscoveredDevice,
    state: &AppState,
) -> bool {
    // First, try to get service URLs from the device's cached capabilities
    let svc_urls = dev
        .capabilities
        .get("service_urls")
        .and_then(|v| {
            serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
        })
        .unwrap_or_default();

    let av_url = svc_urls
        .get("avtransport")
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
    let rc_url = svc_urls
        .get("renderingcontrol")
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
    let cm_url = svc_urls
        .get("connectionmanager")
        .or_else(|| svc_urls.get("ConnectionManager"))
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));

    // If cached service URLs are available, use them
    if let (Some(av), Some(rc)) = (av_url, rc_url) {
        let delay =
            crate::config::resolve_play_delay(&state.backend, &state.config, &dev.id, &dev.name);
        let dlna = DlnaOutput::new(
            dev.name.clone(),
            dev.id.clone(),
            dev.host.clone(),
            av,
            rc,
            cm_url,
        )
        .with_play_delay(delay);
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(dlna));
        info!(name = %dev.name, id = %dev.id, "dlna_output_registered_on_zone_create");
        return true;
    }

    // Fallback: fetch device description from location URL
    if let Some(ref location) = dev.location {
        match fetch_device_description(location).await {
            Ok(desc) => {
                if desc.is_media_renderer() || desc.is_openhome() {
                    let service_urls = desc.service_urls();
                    let av = service_urls.get("avtransport");
                    let rc = service_urls.get("renderingcontrol");
                    if let (Some(av_path), Some(rc_path)) = (av, rc) {
                        let base = format!("http://{}:{}", dev.host, dev.port);
                        let cm_path = service_urls
                            .get("connectionmanager")
                            .or_else(|| service_urls.get("ConnectionManager"))
                            .map(|p| format!("{base}{p}"));
                        let delay = crate::config::resolve_play_delay(
                            &state.backend,
                            &state.config,
                            &dev.id,
                            &dev.name,
                        );
                        let dlna = DlnaOutput::new(
                            dev.name.clone(),
                            dev.id.clone(),
                            dev.host.clone(),
                            format!("{base}{av_path}"),
                            format!("{base}{rc_path}"),
                            cm_path,
                        )
                        .with_play_delay(delay);
                        let mut outputs = state.outputs.lock().await;
                        outputs.register(Box::new(dlna));
                        info!(name = %dev.name, id = %dev.id, "dlna_output_registered_via_description");
                        return true;
                    }
                }
            }
            Err(e) => {
                warn!(device = %dev.name, error = %e, "dlna_description_fetch_failed");
            }
        }
    }

    false
}

/// DELETE /zones — soft-delete every zone and clear the free-tier
/// activation markers, so a Free user whose 3-zone quota is consumed by
/// stale renderers can start over and explicitly re-create the zones he
/// wants (discovery never resurrects hidden zones, only POST /zones does).
async fn delete_all_zones(State(state): State<AppState>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let ids: Vec<i64> = repo
        .list()
        .map(|zs| zs.iter().filter_map(|z| z.id).collect())
        .unwrap_or_default();
    match repo.delete_all() {
        Ok(_) => {
            info!(count = ids.len(), "all_zones_deleted_quota_reset");
            for id in ids {
                state.event_bus.emit_typed(
                    tune_core::event_types::EventType::ZoneDeleted,
                    json!({"id": id}),
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_zone(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneDeleted,
                json!({"id": id}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateVolume>,
) -> impl IntoResponse {
    // Normalise: web client sends 0.0–1.0, legacy clients may send 0–100.
    let volume_f = if body.volume > 1.0 {
        body.volume / 100.0
    } else {
        body.volume
    };
    let volume_int = (volume_f * 100.0).round() as i32;

    // Persist to DB
    let repo = ZoneRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.update_volume(id, volume_int) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    // Forward to the output device (Squeezebox LMS, DLNA, etc.)
    let device_id = repo.get(id).ok().flatten().and_then(|z| z.output_device_id);
    state
        .orchestrator
        .set_volume(id, volume_f, device_id.as_deref())
        .await;

    StatusCode::NO_CONTENT.into_response()
}

async fn update_muted(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMuted>,
) -> impl IntoResponse {
    // Persist to DB
    let repo = ZoneRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.update_muted(id, body.muted) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    // Forward to the output device (Squeezebox LMS, DLNA, etc.)
    let device_id = repo.get(id).ok().flatten().and_then(|z| z.output_device_id);
    state
        .orchestrator
        .set_mute(id, body.muted, device_id.as_deref())
        .await;

    StatusCode::NO_CONTENT.into_response()
}

async fn rename_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.update_name(id, &body.name) {
        Ok(_) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneUpdated,
                json!({ "id": id, "name": body.name }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateGroup {
    name: String,
    zone_ids: Vec<i64>,
}

async fn list_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(groups))
}

async fn create_group(
    State(state): State<AppState>,
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

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = groups.len() as i64 + 1;
    groups.push(json!({
        "id": id,
        "name": body.name,
        "zone_ids": body.zone_ids,
    }));

    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupCreated,
        json!({ "id": id, "name": body.name, "zone_ids": body.zone_ids }),
    );
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

#[derive(Deserialize)]
struct PatchGroup {
    name: Option<String>,
    zone_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
struct GroupVolumeRequest {
    master_volume: Option<f64>,
    offsets: Option<std::collections::HashMap<String, f64>>,
}

async fn patch_group(
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

async fn group_volume(
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

            let repo = ZoneRepo::with_backend(state.backend.clone());
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
            Ok(Json(json!({"group_id": group_id, "master_volume": master})).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn calibrate_group(
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

            // For each zone, measure round-trip latency to its output device
            let outputs = state.outputs.lock().await;
            let mut latencies = Vec::new();
            for zid in &zone_ids {
                let zone = ZoneRepo::with_backend(state.backend.clone())
                    .get(*zid)
                    .ok()
                    .flatten();
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

            // First zone is the leader; compute sync delays relative to it
            let leader_latency = latencies.first().map(|(_, l)| *l).unwrap_or(0);
            let mut calibration = serde_json::Map::new();
            for (zid, lat) in &latencies {
                let sync_delay = leader_latency - lat;
                calibration.insert(zid.to_string(), json!(sync_delay));
            }

            Json(json!({"group_id": group_id, "calibration": calibration})).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn group_health(
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

async fn delete_group(
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

async fn list_stereo_pairs(State(state): State<AppState>) -> Json<Value> {
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
struct CreateStereoPair {
    name: String,
    left_device_id: String,
    right_device_id: String,
}

async fn create_stereo_pair(
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

async fn delete_stereo_pair(
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

async fn list_group_delays(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let raw = settings
        .get("group_delays")
        .unwrap_or(None)
        .unwrap_or_default();
    let delays: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    Json(json!(delays))
}

async fn set_group_delay(
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

#[cfg(test)]
mod signal_path_tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::playback::NowPlaying;

    fn dlna_zone() -> (Arc<dyn DbBackend>, Zone) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        (backend, zone)
    }

    // Hi-res ALAC source, currently playing, with a live stream session.
    fn alac_hires_playing() -> ZoneState {
        let np = NowPlaying {
            title: "Track".into(),
            format: Some("alac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        }
    }

    fn transcoder_desc(v: &Value) -> Option<String> {
        v.get("steps")?
            .as_array()?
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("Transcoder"))
            .and_then(|s| s.get("description").and_then(|d| d.as_str()))
            .map(String::from)
    }

    // Sevy, LHC-52: the renderer is served WAV/LPCM (it does not advertise
    // audio/flac), so the path must show the REAL wire container, not the
    // static ALAC→FLAC transcode guess. The output is 16-bit LPCM, so the
    // hi-res 24-bit source reads as downconverted (not bit-perfect).
    #[test]
    fn dlna_wav_wire_shows_alac_to_wav() {
        let (backend, zone) = dlna_zone();
        let ps = alac_hires_playing();
        let sp =
            build_signal_path(&ps, &zone, &backend, Some("LHC-52"), "none", Some("wav")).unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} WAV 96kHz/16bit")
        );
        // Hi-res source truncated to the 16-bit LPCM cap → not bit-perfect,
        // but still a lossless source.
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
        assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
    }

    // Regression guard: with no live session container (None) the display keeps
    // its prior behaviour — ALAC transcodes to FLAC for DLNA.
    #[test]
    fn dlna_without_session_keeps_flac_target() {
        let (backend, zone) = dlna_zone();
        let ps = alac_hires_playing();
        let sp = build_signal_path(&ps, &zone, &backend, Some("LHC-52"), "none", None).unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/24bit")
        );
    }

    // A FLAC-advertising renderer (wire = flac) is unaffected by the override.
    #[test]
    fn dlna_flac_wire_keeps_flac_target() {
        let (backend, zone) = dlna_zone();
        let ps = alac_hires_playing();
        let sp =
            build_signal_path(&ps, &zone, &backend, Some("Node"), "none", Some("flac")).unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/24bit")
        );
    }

    // Native WAV 24-bit source, served byte-for-byte over the WAV wire.
    fn wav24_playing() -> ZoneState {
        let np = NowPlaying {
            title: "Track".into(),
            format: Some("wav".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        }
    }

    // Sandro/Progman: a NATIVE WAV 24-bit source is passthrough (WAV never
    // transcodes for DLNA), so it must read bit-perfect even with dlna_wav24 off
    // — the badge previously showed red for WAV 24-bit direct.
    #[test]
    fn dlna_native_wav24_is_bit_perfect() {
        let (backend, zone) = dlna_zone();
        let ps = wav24_playing();
        let sp =
            build_signal_path(&ps, &zone, &backend, Some("Diretta"), "none", Some("wav")).unwrap();
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
        assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
    }

    #[test]
    fn wav_wire_native_wav_is_bit_perfect_any_depth() {
        assert!(wav_wire_bit_perfect(true, true, false, 24)); // native WAV 24-bit, flag off
        assert!(wav_wire_bit_perfect(true, true, false, 16));
    }

    #[test]
    fn wav_wire_flac_fallback_capped_at_16_bit() {
        // FLAC/ALAC → WAV fallback (source not WAV): 24-bit needs the override.
        assert!(!wav_wire_bit_perfect(true, false, false, 24));
        assert!(wav_wire_bit_perfect(true, false, false, 16)); // fits plain 16-bit LPCM
        assert!(wav_wire_bit_perfect(true, false, true, 24)); // dlna_wav24 preserves 24-bit
    }

    #[test]
    fn wav_wire_lossy_source_never_bit_perfect() {
        assert!(!wav_wire_bit_perfect(false, true, true, 16));
    }
}
