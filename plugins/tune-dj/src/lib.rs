//! DJ mode as a native [`TunePlugin`] (#917).
//!
//! Extracted verbatim from `tune-server`'s always-on core (`routes/dj.rs`) so
//! the stock server no longer carries it: build `tune-server --features dj` to
//! get these routes back, mounted by the plugin host at
//! `/api/v1/ext/dj/…` (the host derives the prefix from `name()` — a plugin
//! never chooses its own).
//!
//! DJ is **native**, not WASM: `waveform`/`analyze` need full audio access and
//! call [`tune_core::audio::decode::decode_to_pcm`] directly.
//!
//! Host dependencies are passed explicitly at construction via [`HostServices`]
//! — matching the wiring pattern documented in `tune-server/src/plugins.rs`, so
//! a plugin's real dependencies are visible at the registration site. DJ only
//! needs the DB backend (settings + track lookups); its router captures that
//! backend in its own state rather than sharing the host's `AppState`, which
//! keeps `tune-core` free of any `tune-server` type.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::audio::decode::decode_to_pcm;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

/// Host services handed to the DJ plugin at construction.
///
/// Passed explicitly (not pulled from [`PluginContext`]) so the plugin's real
/// dependencies are visible where it is wired up in
/// `register_builtin_plugins`. DJ needs only the DB backend.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
}

/// The DJ plugin. Owns the DB backend its router needs.
pub struct DjPlugin {
    backend: Arc<dyn DbBackend>,
}

impl DjPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            backend: services.backend,
        }
    }
}

#[async_trait]
impl TunePlugin for DjPlugin {
    fn name(&self) -> &str {
        "dj"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "DJ mode: crossfade, decks, waveform and BPM analysis"
    }
    // Opt-in: a niche mode that stays dormant until the user installs it from
    // the plugin manager, rather than running for everyone by default.
    fn default_enabled(&self) -> bool {
        false
    }

    // Hors catalogue (#2090). Le gestionnaire ne doit pas proposer d'installer
    // DJ, parce que DJ ne fait pas ce que sa description annonce.
    //
    // « crossfade, decks » : le greffon ne reçoit QUE la base
    // (`HostServices { backend }`) — ni `PlaybackManager`, ni registre de
    // sorties. Il n'a aucun accès au chemin audio, donc aucun moyen de faire
    // jouer, de fondre ou de charger une platine, quel que soit le contenu des
    // handlers. Et de fait, sur les 13 routes déclarées plus bas, 11 ne
    // changent rien :
    //
    //   * 7 renvoient l'argument reçu sans rien écrire — `play`, `pause`,
    //     `crossfade`, `crossfader`, `auto-crossfade`, `load`, `volume` ;
    //   * `sync-tempo` répond littéralement « tempo sync not yet implemented » ;
    //   * `enable`, `disable` et `status` n'écrivent et ne relisent que
    //     `dj_enabled_{zone}`, un réglage dont ces trois handlers sont les
    //     SEULS lecteurs du dépôt. `status` renvoie par-dessus des platines
    //     toujours `loaded: false` et un `crossfader: 0.5` en dur — il contredit
    //     donc `load` et `crossfader` juste après leur succès annoncé.
    //
    // Restent 2 routes qui travaillent vraiment : `waveform` et `analyze`
    // (décodage PCM natif).
    // Elles restent servies : le greffon est toujours compilé, toujours testé
    // (`tests/dj_plugin.rs`), et se charge encore si l'on pose
    // `plugin_dj_installed=true` à la main. Ce qui cesse, c'est la promesse.
    //
    // À rebasculer à `true` le jour où les platines existent pour de bon.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.backend.clone()));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// DJ reacts to no events today — auto-crossfade is a client-driven poke at
    /// `/auto-crossfade`, not a server-side hook. Left as a no-op override so the
    /// plugin does not receive every event on the bus for nothing.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

/// Plugin-owned router state. Captures the host's DB backend so the router can
/// be a `Router<()>` (as the host requires) without leaking `AppState`.
#[derive(Clone)]
struct DjState {
    backend: Arc<dyn DbBackend>,
}

/// The DJ router, `Router<()>` for the plugin host to mount under
/// `/api/v1/ext/dj`. Routes are identical to the old `routes/dj.rs`.
pub fn router(backend: Arc<dyn DbBackend>) -> Router<()> {
    Router::new()
        .route("/enable/{zone_id}", post(enable_dj))
        .route("/disable/{zone_id}", post(disable_dj))
        .route("/status/{zone_id}", get(dj_status))
        .route("/play", post(dj_play))
        .route("/pause", post(dj_pause))
        .route("/crossfade", post(dj_crossfade))
        .route("/crossfader", post(dj_crossfader))
        .route("/auto-crossfade", post(dj_auto_crossfade))
        .route("/load/{zone_id}/{deck}", post(dj_load))
        .route("/volume/{zone_id}/{deck}", post(dj_volume))
        .route("/sync-tempo/{zone_id}", post(dj_sync_tempo))
        .route("/waveform/{track_id}", get(dj_waveform))
        .route("/analyze/{track_id}", post(dj_analyze))
        .with_state(DjState { backend })
}

async fn enable_dj(State(state): State<DjState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set(&format!("dj_enabled_{zone_id}"), "true").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": true}))
}

async fn disable_dj(State(state): State<DjState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set(&format!("dj_enabled_{zone_id}"), "false").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": false}))
}

async fn dj_status(State(state): State<DjState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get(&format!("dj_enabled_{zone_id}"))
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    Json(json!({
        "zone_id": zone_id,
        "dj_mode": enabled,
        "deck_a": {"loaded": false, "track": null, "position_ms": 0, "bpm": null},
        "deck_b": {"loaded": false, "track": null, "position_ms": 0, "bpm": null},
        "crossfader": 0.5,
        "auto_crossfade": false,
    }))
}

#[derive(Deserialize)]
struct DjPlayRequest {
    zone_id: i64,
}

async fn dj_play(Json(body): Json<DjPlayRequest>) -> Json<Value> {
    Json(json!({"zone_id": body.zone_id, "playing": true}))
}

async fn dj_pause(Json(body): Json<DjPlayRequest>) -> Json<Value> {
    Json(json!({"zone_id": body.zone_id, "playing": false}))
}

#[derive(Deserialize)]
struct CrossfadeRequest {
    zone_id: i64,
    duration_ms: Option<i64>,
}

async fn dj_crossfade(Json(body): Json<CrossfadeRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "crossfade_started": true,
        "duration_ms": body.duration_ms.unwrap_or(5000),
    }))
}

#[derive(Deserialize)]
struct CrossfaderRequest {
    zone_id: i64,
    position: f64,
}

async fn dj_crossfader(Json(body): Json<CrossfaderRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "crossfader": body.position.clamp(0.0, 1.0),
    }))
}

#[derive(Deserialize)]
struct AutoCrossfadeRequest {
    zone_id: i64,
    enabled: bool,
    duration_ms: Option<i64>,
}

async fn dj_auto_crossfade(Json(body): Json<AutoCrossfadeRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "auto_crossfade": body.enabled,
        "duration_ms": body.duration_ms.unwrap_or(5000),
    }))
}

#[derive(Deserialize)]
struct LoadDeckRequest {
    track_id: i64,
}

async fn dj_load(
    Path((zone_id, deck)): Path<(i64, String)>,
    Json(body): Json<LoadDeckRequest>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "deck": deck,
        "track_id": body.track_id,
        "loaded": true,
    }))
}

#[derive(Deserialize)]
struct DeckVolumeRequest {
    volume: f64,
}

async fn dj_volume(
    Path((zone_id, deck)): Path<(i64, String)>,
    Json(body): Json<DeckVolumeRequest>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "deck": deck,
        "volume": body.volume.clamp(0.0, 1.0),
    }))
}

async fn dj_sync_tempo(Path(zone_id): Path<i64>) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "synced": true,
        "message": "tempo sync not yet implemented",
    }))
}

async fn dj_waveform(State(state): State<DjState>, Path(track_id): Path<i64>) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = repo.get(track_id).ok().flatten();
    let Some(track) = track else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "track not found"})),
        )
            .into_response();
    };
    let Some(ref path) = track.file_path else {
        return Json(json!({"track_id": track_id, "error": "no file path"})).into_response();
    };

    // Decode to mono PCM natively, then downsample to ~8kHz equivalent by striding
    let path_owned = path.clone();
    let decoded =
        tokio::task::spawn_blocking(move || decode_to_pcm(&path_owned, None, Some(1), 0.0, 0.0))
            .await;

    match decoded {
        Ok(Ok(audio)) if !audio.samples_i32.is_empty() => {
            let source_rate = audio.sample_rate as usize;
            let scale = match audio.bit_depth {
                24 => (1i64 << 23) as f32,
                32 => (1i64 << 31) as f32,
                _ => 32768.0,
            };
            // Stride factor to approximate 8kHz from native rate
            let stride = (source_rate / 8000).max(1);
            let samples: Vec<f32> = audio
                .samples_i32
                .iter()
                .step_by(stride)
                .map(|&s| s as f32 / scale)
                .collect();

            // Downsample to ~200 points (peak amplitude per chunk)
            let target_points = 200usize;
            let chunk_size = (samples.len() / target_points).max(1);
            let waveform: Vec<f32> = samples
                .chunks(chunk_size)
                .map(|chunk| chunk.iter().map(|s| s.abs()).fold(0.0f32, f32::max))
                .collect();

            Json(json!({
                "track_id": track_id,
                "points": waveform.len(),
                "waveform": waveform,
            }))
            .into_response()
        }
        _ => Json(json!({
            "track_id": track_id,
            "waveform": null,
            "error": "native decode failed",
        }))
        .into_response(),
    }
}

async fn dj_analyze(State(state): State<DjState>, Path(track_id): Path<i64>) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = repo.get(track_id).ok().flatten();
    let Some(track) = track else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "track not found"})),
        )
            .into_response();
    };
    let Some(ref path) = track.file_path else {
        return Json(json!({"track_id": track_id, "error": "no file path"})).into_response();
    };

    // Decode to mono PCM natively for energy-based beat detection
    let path_owned = path.clone();
    let decoded =
        tokio::task::spawn_blocking(move || decode_to_pcm(&path_owned, None, Some(1), 0.0, 0.0))
            .await;

    match decoded {
        Ok(Ok(audio)) if !audio.samples_i32.is_empty() => {
            let source_rate = audio.sample_rate as usize;
            let scale = match audio.bit_depth {
                24 => (1i64 << 23) as f32,
                32 => (1i64 << 31) as f32,
                _ => 32768.0,
            };
            // Stride to approximate 22050 Hz from native rate
            let stride = (source_rate / 22050).max(1);
            let effective_rate: usize = source_rate / stride;

            let samples: Vec<f32> = audio
                .samples_i32
                .iter()
                .step_by(stride)
                .map(|&s| s as f32 / scale)
                .collect();

            // 250 ms windows for energy computation
            let window_size = effective_rate / 4;
            if window_size == 0 {
                return Json(json!({
                    "track_id": track_id,
                    "bpm": null,
                    "error": "audio too short for analysis",
                }))
                .into_response();
            }

            let energies: Vec<f32> = samples
                .chunks(window_size)
                .map(|chunk| chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32)
                .collect();

            if energies.len() < 4 {
                return Json(json!({
                    "track_id": track_id,
                    "bpm": null,
                    "error": "audio too short for analysis",
                }))
                .into_response();
            }

            let avg_energy: f32 = energies.iter().sum::<f32>() / energies.len() as f32;
            let threshold = avg_energy * 1.3;

            // Count onset peaks (energy crossing above threshold)
            let mut beats = 0u32;
            let mut prev_above = false;
            for &e in &energies {
                let above = e > threshold;
                if above && !prev_above {
                    beats += 1;
                }
                prev_above = above;
            }

            let duration_secs = samples.len() as f64 / effective_rate as f64;
            let bpm_raw = if duration_secs > 0.0 {
                (beats as f64 / duration_secs * 60.0).round()
            } else {
                0.0
            };
            // Only report BPM in plausible range
            let bpm = if (60.0..=200.0).contains(&bpm_raw) {
                Some(bpm_raw)
            } else {
                None
            };

            Json(json!({
                "track_id": track_id,
                "bpm": bpm,
                "duration_s": duration_secs.round(),
                "beats_detected": beats,
            }))
            .into_response()
        }
        _ => Json(json!({
            "track_id": track_id,
            "bpm": null,
            "error": "native decode failed",
        }))
        .into_response(),
    }
}
