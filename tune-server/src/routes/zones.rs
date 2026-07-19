use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::audio::formats::AudioFormat;
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
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_zones).post(create_zone))
        .route("/{id}", get(get_zone).patch(patch_zone).delete(delete_zone))
        .route("/{id}/volume", put(update_volume))
        .route("/{id}/muted", put(update_muted))
        .route("/{id}/dsp", get(get_zone_dsp).put(set_zone_dsp))
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

    match repo.get_dsp_config(id) {
        Ok((preset_id, enabled)) => Json(json!({
            "zone_id": id,
            "dsp_preset_id": preset_id,
            "dsp_enabled": enabled,
            "eq_profile": eq_profile.unwrap_or_default(),
        }))
        .into_response(),
        Err(_) => Json(json!({
            "zone_id": id,
            "eq_profile": eq_profile.unwrap_or_default(),
        }))
        .into_response(),
    }
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

    let preset_id = body["dsp_preset_id"].as_i64();
    let enabled = body["dsp_enabled"].as_bool().unwrap_or(false);
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let _ = repo.update_dsp(id, preset_id, enabled);

    Json(json!({
        "zone_id": id,
        "dsp_preset_id": preset_id,
        "dsp_enabled": enabled,
        "eq_profile": body.get("eq_profile"),
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
pub fn build_signal_path_pub(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
) -> Option<Value> {
    build_signal_path(ps, zone, backend, renderer_label, audio_backend)
}

/// Build the `signal_path` object for a zone's current playback.
/// Returns `None` when the zone is not playing.
///
/// `audio_backend` is the active audio backend name ("ASIO", "WASAPI",
/// "CoreAudio", "ALSA") used for local zones' signal path display.
fn build_signal_path(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
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
        if l.contains("aac") || l.contains("mp4") || l.contains("m4a") {
            "AAC"
        } else if l.contains("mp3") || l.contains("mpeg") {
            "MP3"
        } else if l.contains("flac") {
            "FLAC"
        } else if l.contains("alac") {
            "ALAC"
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
    let is_lossless = source_format.as_ref().is_some_and(|f| f.is_lossless());

    let output_type = zone.output_type.as_deref().unwrap_or("local");

    // Determine if DSP is active
    let dsp_enabled = ZoneRepo::with_backend(backend.clone())
        .get_dsp_config(zone.id.unwrap_or(0))
        .map(|(preset_id, enabled)| enabled && preset_id.is_some())
        .unwrap_or(false);

    // Volume at 100% means no software volume adjustment.
    // Fixed-volume zones always output at full volume (bit-perfect).
    let volume_full = zone.fixed_volume || ps.volume >= 1.0 || ps.volume <= 0.0; // 0.0 means no software vol set

    // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC) for network outputs.
    // FLAC, WAV, MP3, AAC are natively supported and pass through without transcoding.
    let is_network_output = matches!(
        output_type,
        "dlna" | "openhome" | "chromecast" | "bluos" | "squeezebox"
    );
    let needs_transcode_for_output = is_network_output
        && source_format
            .as_ref()
            .is_some_and(|f| f.needs_transcode_for_dlna());
    // OAAT transcodes everything to WAV except WAV itself
    let is_oaat = output_type == "oaat";
    let oaat_transcodes = is_oaat
        && source_format
            .as_ref()
            .is_some_and(|f| *f != AudioFormat::Wav);

    let (transport_bit_perfect, transport_desc, output_format_name) = match output_type {
        "dlna" | "openhome" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
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

    // Transcoding step (only if transcoding occurs)
    let transcode_active =
        needs_transcode_for_output || oaat_transcodes || output_type == "airplay";
    if transcode_active {
        // OAAT lossless PCM → WAV preserves all audio data, but DSD → WAV is a
        // lossy domain conversion (see the "oaat" transport arm above).
        let transcode_lossless = is_oaat && is_lossless && !is_dsd;
        steps.push(json!({
            "name": "Transcoder",
            "description": format!("{format_name} \u{2192} {output_format_name}"),
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
        tune_core::outputs::local::active_backend_name(&state.config.local_audio_backend);
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
            let signal_path =
                build_signal_path(&ps, z, &state.backend, renderer_label, audio_backend);
            obj.insert("signal_path".into(), json!(signal_path));
            obj.insert("is_default".into(), json!(default_zone_id == Some(zone_id)));
            let zone_repo = ZoneRepo::with_backend(state.backend.clone());
            obj.insert("dsd_mode".into(), json!(zone_repo.get_dsd_mode(zone_id)));
            obj.insert(
                "dlna_native_flac".into(),
                json!(zone_repo.get_dlna_native_flac(zone_id)),
            );
            obj.insert(
                "alac_passthrough".into(),
                json!(zone_repo.get_alac_passthrough(zone_id)),
            );
            obj.insert("dlna_lpcm".into(), json!(zone_repo.get_dlna_lpcm(zone_id)));
            let online = match z.output_type.as_deref() {
                Some("local") | Some("browser") => true,
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
        tune_core::outputs::local::active_backend_name(&state.config.local_audio_backend);
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
                let signal_path =
                    build_signal_path(&ps, &zone, &state.backend, renderer_label, audio_backend);
                obj.insert("signal_path".into(), json!(signal_path));
                obj.insert("dsd_mode".into(), json!(repo.get_dsd_mode(id)));
                obj.insert(
                    "dlna_native_flac".into(),
                    json!(repo.get_dlna_native_flac(id)),
                );
                obj.insert(
                    "alac_passthrough".into(),
                    json!(repo.get_alac_passthrough(id)),
                );
                obj.insert("dlna_lpcm".into(), json!(repo.get_dlna_lpcm(id)));
                let online = match zone.output_type.as_deref() {
                    Some("local") | Some("browser") => true,
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
        let delay = state.config.play_delay_for(&dev.name);
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
                        let delay = state.config.play_delay_for(&dev.name);
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
