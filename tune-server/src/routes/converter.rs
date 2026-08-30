use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use tune_core::audio::decode::{can_decode_native, decode_to_pcm};
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ConvertSource {
    pub track_id: Option<i64>,
    /// Whole album: expanded to all of its tracks' files. The web Converter
    /// selects albums, so this is the common case (Reivax66/Bilou, #1094/#1095 —
    /// the web sent album ids the server didn't accept → 422).
    pub album_id: Option<i64>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartJobRequest {
    pub sources: Vec<ConvertSource>,
    pub format: String,
    pub quality: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
struct JobError {
    file: String,
    message: String,
}

struct ConvertJob {
    status: JobStatus,
    total: usize,
    completed: usize,
    current_file: String,
    errors: Vec<JobError>,
    output_dir: PathBuf,
}

type JobStore = Arc<Mutex<HashMap<String, Arc<Mutex<ConvertJob>>>>>;

/// Lazily initialised per-process job store.  We store it as a layer extension
/// so it lives as long as the router.
fn job_store() -> JobStore {
    /// Global singleton — `OnceLock` ensures we create exactly one map even if
    /// `router()` is called more than once (which shouldn't happen, but be safe).
    static STORE: std::sync::OnceLock<JobStore> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/start", post(start_job))
        .route("/status/{job_id}", get(job_status))
        .route("/download/{job_id}", get(download_job))
        .route("/presets", get(list_presets))
        .route("/capabilities", get(capabilities))
        .route("/jobs/{job_id}", delete(cancel_job))
}

// ---------------------------------------------------------------------------
// GET /capabilities — which formats THIS machine can actually produce
// ---------------------------------------------------------------------------

/// The web client used to offer all six formats blind: on a machine without
/// the external tools, four of the six choices ended in an error after the
/// user had already picked files and clicked start (#1524). This endpoint
/// tells the UI what to grey out — and why.
///
/// ffmpeg presence is not enough: the minimal build bundled with the release
/// carries only the `aac` encoder (no libmp3lame), so mp3 must be answered
/// from what the resolved binary actually encodes.
async fn capabilities() -> impl IntoResponse {
    let ffmpeg = resolve_tool("ffmpeg");
    let lame = resolve_tool("lame");
    let encoders = match &ffmpeg {
        Some(path) => ffmpeg_encoders(path).await,
        None => std::collections::HashSet::new(),
    };

    Json(json!({
        // Native formats are always available: flac/wav/opus (#1525),
        // alac via Apple's vendored encoder (#1526), aac via the OS
        // encoder where one exists (#1527 — AudioToolbox on macOS).
        "formats": {
            "flac": true,
            "wav": true,
            "opus": true,
            "alac": true,
            "mp3": lame.is_some() || encoders.contains("libmp3lame"),
            "aac": tune_core::audio::aac_encoder::native_available()
                || encoders.contains("aac"),
        },
        // Diagnostic detail: which tool backs the non-native formats, if any.
        "tools": {
            "ffmpeg": ffmpeg.map(|p| p.display().to_string()),
            "lame": lame.map(|p| p.display().to_string()),
        },
    }))
}

/// Ask the resolved ffmpeg what it can encode (`ffmpeg -encoders`), cached
/// for the process lifetime — the binary next to the executable does not
/// change while we run.
async fn ffmpeg_encoders(path: &Path) -> std::collections::HashSet<String> {
    static CACHE: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    if let Some(cached) = CACHE.get() {
        return cached.clone();
    }
    let out = tokio::process::Command::new(path)
        .args(["-hide_banner", "-encoders"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    let set = match out {
        Ok(o) if o.status.success() => parse_ffmpeg_encoders(&String::from_utf8_lossy(&o.stdout)),
        _ => std::collections::HashSet::new(),
    };
    CACHE.get_or_init(|| set).clone()
}

/// Parse `ffmpeg -encoders` output: after the `------` separator, each line
/// is ` <flags> <name> <description>` — the name is the second column.
fn parse_ffmpeg_encoders(stdout: &str) -> std::collections::HashSet<String> {
    stdout
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// POST /start — kick off a batch conversion
// ---------------------------------------------------------------------------

async fn start_job(
    State(state): State<AppState>,
    Json(body): Json<StartJobRequest>,
) -> Result<axum::response::Response, AppError> {
    // Premium gate: batch converter requires Premium
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::BatchConverter,
    )
    .await
    {
        return Ok(resp);
    }

    // Validate format
    let format = body.format.to_lowercase();
    if !matches!(
        format.as_str(),
        "flac" | "wav" | "mp3" | "aac" | "alac" | "opus"
    ) {
        return Err(AppError::bad_request(format!(
            "unsupported format: {format}"
        )));
    }

    // Resolve all source paths
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut file_paths: Vec<PathBuf> = Vec::new();

    for src in &body.sources {
        if let Some(track_id) = src.track_id {
            match repo.get(track_id) {
                Ok(Some(track)) => {
                    if let Some(ref fp) = track.file_path {
                        file_paths.push(PathBuf::from(fp));
                    } else {
                        warn!(track_id, "converter_skip_no_file_path");
                    }
                }
                Ok(None) => {
                    warn!(track_id, "converter_skip_track_not_found");
                }
                Err(e) => {
                    warn!(track_id, error = %e, "converter_skip_track_lookup_error");
                }
            }
        } else if let Some(album_id) = src.album_id {
            match repo.list_by_album(album_id) {
                Ok(tracks) => {
                    for t in tracks {
                        if let Some(ref fp) = t.file_path {
                            file_paths.push(PathBuf::from(fp));
                        }
                    }
                }
                Err(e) => {
                    warn!(album_id, error = %e, "converter_skip_album_lookup_error");
                }
            }
        } else if let Some(ref path) = src.path {
            let p = PathBuf::from(path);
            if p.is_dir() {
                collect_audio_files(&p, &mut file_paths);
            } else if p.is_file() && convertible_input(path) {
                file_paths.push(p);
            } else {
                warn!(path, "converter_skip_not_audio_or_missing");
            }
        }
    }

    if file_paths.is_empty() {
        return Err(AppError::bad_request("no audio files found in sources"));
    }

    let total = file_paths.len();
    let job_id = uuid::Uuid::new_v4().to_string();
    let output_dir = PathBuf::from(format!("/tmp/tune-convert/{}", job_id));
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|e| AppError::internal(format!("failed to create output dir: {e}")))?;

    let job = Arc::new(Mutex::new(ConvertJob {
        status: JobStatus::Running,
        total,
        completed: 0,
        current_file: String::new(),
        errors: Vec::new(),
        output_dir: output_dir.clone(),
    }));

    let store = job_store();
    {
        let mut map = store.lock().await;
        map.insert(job_id.clone(), job.clone());
    }

    // Spawn the background worker
    let jid = job_id.clone();
    let fmt = format.clone();
    let quality = body.quality.clone();
    let target_sr = body.sample_rate;
    let target_bd = body.bit_depth;

    tokio::spawn(async move {
        run_conversion(
            job,
            file_paths,
            &fmt,
            quality.as_deref(),
            target_sr,
            target_bd,
            &output_dir,
        )
        .await;
        info!(job_id = %jid, "converter_job_finished");
    });

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "job_id": job_id,
            "total_tracks": total,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /status/{job_id}
// ---------------------------------------------------------------------------

async fn job_status(AxumPath(job_id): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let store = job_store();
    let map = store.lock().await;
    let job_arc = map
        .get(&job_id)
        .ok_or_else(|| AppError::not_found(format!("job not found: {job_id}")))?
        .clone();
    let job = job_arc.lock().await;

    let errors: Vec<Value> = job
        .errors
        .iter()
        .map(|e| json!({"file": e.file, "message": e.message}))
        .collect();

    Ok(Json(json!({
        "job_id": job_id,
        "status": job.status.as_str(),
        "total": job.total,
        "completed": job.completed,
        "current_file": job.current_file,
        "errors": errors,
    })))
}

// ---------------------------------------------------------------------------
// GET /download/{job_id} — stream a ZIP of the converted files
// ---------------------------------------------------------------------------

async fn download_job(AxumPath(job_id): AxumPath<String>) -> Result<impl IntoResponse, AppError> {
    let store = job_store();
    let map = store.lock().await;
    let job_arc = map
        .get(&job_id)
        .ok_or_else(|| AppError::not_found(format!("job not found: {job_id}")))?
        .clone();
    let job = job_arc.lock().await;

    if job.status == JobStatus::Running {
        return Err(AppError::bad_request("job is still running"));
    }

    let output_dir = job.output_dir.clone();
    drop(job);
    drop(map);

    // Build the ZIP in memory (converted files should be reasonably sized)
    let zip_bytes = tokio::task::spawn_blocking(move || build_zip(&output_dir))
        .await
        .map_err(|e| AppError::internal(format!("zip task join error: {e}")))?
        .map_err(|e| AppError::internal(format!("zip build error: {e}")))?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/zip"));
    headers.insert(
        "Content-Disposition",
        HeaderValue::from_str(&format!(
            "attachment; filename=\"tune-convert-{job_id}.zip\""
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"converted.zip\"")),
    );

    Ok((StatusCode::OK, headers, Body::from(zip_bytes)))
}

// ---------------------------------------------------------------------------
// GET /presets
// ---------------------------------------------------------------------------

async fn list_presets() -> Json<Value> {
    Json(json!([
        {
            "id": "flac-cd",
            "label": "CD Quality (FLAC 16/44.1)",
            "format": "flac",
            "quality": "5",
            "sample_rate": 44100,
            "bit_depth": 16
        },
        {
            "id": "flac-hires",
            "label": "Hi-Res (FLAC 24-bit, original sample rate)",
            "format": "flac",
            "quality": "5",
            "sample_rate": null,
            "bit_depth": 24
        },
        {
            "id": "mp3-320",
            "label": "MP3 CBR 320 kbps",
            "format": "mp3",
            "quality": "320",
            "sample_rate": null,
            "bit_depth": null
        },
        {
            "id": "mp3-v0",
            "label": "MP3 VBR V0 (~245 kbps)",
            "format": "mp3",
            "quality": "v0",
            "sample_rate": null,
            "bit_depth": null
        },
        {
            "id": "mp3-192",
            "label": "MP3 CBR 192 kbps",
            "format": "mp3",
            "quality": "192",
            "sample_rate": null,
            "bit_depth": null
        },
        {
            "id": "opus-128",
            "label": "Opus 128 kbps",
            "format": "opus",
            "quality": "128",
            "sample_rate": null,
            "bit_depth": null
        },
        {
            "id": "opus-192",
            "label": "Opus 192 kbps",
            "format": "opus",
            "quality": "192",
            "sample_rate": null,
            "bit_depth": null
        },
        {
            "id": "wav-cd",
            "label": "WAV 16/44.1 (uncompressed)",
            "format": "wav",
            "quality": null,
            "sample_rate": 44100,
            "bit_depth": 16
        },
        {
            "id": "alac-cd",
            "label": "ALAC 16/44.1 (Apple Lossless)",
            "format": "alac",
            "quality": null,
            "sample_rate": 44100,
            "bit_depth": 16
        }
    ]))
}

// ---------------------------------------------------------------------------
// DELETE /jobs/{job_id}
// ---------------------------------------------------------------------------

async fn cancel_job(AxumPath(job_id): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let store = job_store();
    let mut map = store.lock().await;
    let job_arc = map
        .get(&job_id)
        .ok_or_else(|| AppError::not_found(format!("job not found: {job_id}")))?
        .clone();

    {
        let mut job = job_arc.lock().await;
        if job.status == JobStatus::Running {
            job.status = JobStatus::Cancelled;
        }
        // Clean up output directory
        let dir = job.output_dir.clone();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        });
    }

    map.remove(&job_id);

    Ok(Json(json!({
        "job_id": job_id,
        "status": "cancelled",
    })))
}

// ---------------------------------------------------------------------------
// Background conversion worker
// ---------------------------------------------------------------------------

async fn run_conversion(
    job: Arc<Mutex<ConvertJob>>,
    files: Vec<PathBuf>,
    format: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
    output_dir: &Path,
) {
    for file_path in &files {
        // Check if cancelled
        {
            let j = job.lock().await;
            if j.status == JobStatus::Cancelled {
                return;
            }
        }

        let filename = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("track")
            .to_string();

        {
            let mut j = job.lock().await;
            j.current_file = file_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
        }

        let ext = output_extension(format);
        let out_path = output_dir.join(format!("{filename}.{ext}"));

        match convert_single_file(file_path, &out_path, format, quality, target_sr, target_bd).await
        {
            Ok(()) => {
                // Copy tags from source to output
                if let Err(e) = copy_tags(file_path, &out_path) {
                    warn!(
                        src = %file_path.display(),
                        dst = %out_path.display(),
                        error = %e,
                        "converter_copy_tags_failed"
                    );
                }

                let mut j = job.lock().await;
                j.completed += 1;
            }
            Err(e) => {
                error!(
                    file = %file_path.display(),
                    error = %e,
                    "converter_file_failed"
                );
                let mut j = job.lock().await;
                j.completed += 1;
                j.errors.push(JobError {
                    file: file_path.display().to_string(),
                    message: e,
                });
            }
        }
    }

    let mut j = job.lock().await;
    if j.status == JobStatus::Running {
        j.status = if j.errors.len() == j.total {
            JobStatus::Failed
        } else {
            JobStatus::Completed
        };
    }
    j.current_file.clear();
}

// ---------------------------------------------------------------------------
// Single-file conversion
// ---------------------------------------------------------------------------

async fn convert_single_file(
    input: &Path,
    output: &Path,
    format: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    let input_str = input
        .to_str()
        .ok_or_else(|| "invalid input path".to_string())?;

    // mp3 — and aac where the OS has no system encoder — still shell out
    // (chantier #1523: lot 1 ships ffmpeg with the release). Everything
    // else is fully in-process: flac/wav + resampling (rubato, #1525), opus
    // (libopus + native Ogg mux, #1525), alac (Apple's vendored encoder +
    // native m4a mux, #1526), aac via AudioToolbox on macOS (#1527).
    let needs_external = match format {
        "mp3" => true,
        "aac" => !tune_core::audio::aac_encoder::native_available(),
        _ => false,
    };
    if needs_external {
        return encode_with_external(input_str, output, format, quality, target_sr, target_bd)
            .await;
    }

    let input_owned = input_str.to_string();
    let format_owned = format.to_string();
    let quality_owned = quality.map(str::to_string);
    let sr = target_sr;
    let bd = target_bd;
    let output_owned = output.to_path_buf();

    tokio::task::spawn_blocking(move || match format_owned.as_str() {
        "opus" => encode_opus_native(&input_owned, &output_owned, quality_owned.as_deref()),
        "alac" => encode_alac_native(&input_owned, &output_owned, sr, bd),
        "aac" => encode_aac_native(&input_owned, &output_owned, quality_owned.as_deref(), sr),
        _ => encode_lossless_native(&input_owned, &output_owned, &format_owned, sr, bd),
    })
    .await
    .map_err(|e| format!("spawn_blocking join error: {e}"))?
}

/// Encode to AAC (.m4a) via the OS encoder (#1527, macOS/AudioToolbox):
/// native decode → rubato to a standard AAC rate → system encoder + native
/// m4a mux. Same quality contract as the ffmpeg path (bitrate in kb/s).
fn encode_aac_native(
    input: &str,
    output: &Path,
    quality: Option<&str>,
    target_sr: Option<u32>,
) -> Result<(), String> {
    let decoded = decode_for_convert(input, target_sr)?;

    // AAC wants a standard rate; honour the request when it is one, and
    // fall back to 48 kHz otherwise (hi-res sources included).
    let out_sr = target_sr
        .or(Some(decoded.sample_rate))
        .filter(|&sr| tune_core::audio::aac_encoder::rate_supported(sr))
        .unwrap_or(48000);
    let samples = if out_sr != decoded.sample_rate {
        tune_core::audio::resample::resample_i32(
            &decoded.samples_i32,
            decoded.bit_depth,
            decoded.channels as u16,
            decoded.sample_rate,
            out_sr,
        )
    } else {
        decoded.samples_i32.clone()
    };

    // System encoder input is i16.
    let shift = decoded.bit_depth.saturating_sub(16);
    let pcm16: Vec<i16> = samples
        .iter()
        .map(|&s| (s >> shift).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
        .collect();

    let bitrate_kbps: u32 = quality.and_then(|q| q.parse().ok()).unwrap_or(256);
    let bytes = tune_core::audio::aac_encoder::encode_aac_m4a(
        &pcm16,
        decoded.channels as u16,
        out_sr,
        bitrate_kbps * 1000,
    )?;
    std::fs::write(output, &bytes).map_err(|e| format!("failed to write {}: {e}", output.display()))
}

/// Encode to ALAC (.m4a) fully in-process (#1526): native decode → rubato
/// if a target rate is asked → Apple's vendored encoder + native m4a mux.
/// Replaces the ffmpeg subprocess — ALAC can no longer be missing.
fn encode_alac_native(
    input: &str,
    output: &Path,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    let decoded = decode_for_convert(input, target_sr)?;

    let out_sr = target_sr.unwrap_or(decoded.sample_rate);
    let samples = if out_sr != decoded.sample_rate {
        tune_core::audio::resample::resample_i32(
            &decoded.samples_i32,
            decoded.bit_depth,
            decoded.channels as u16,
            decoded.sample_rate,
            out_sr,
        )
    } else {
        decoded.samples_i32.clone()
    };

    // ALAC takes 16/24/32-bit input; honour an explicit bit-depth request,
    // and lift any other depth to the nearest supported one.
    let out_bd = match target_bd.unwrap_or(decoded.bit_depth) {
        d if d <= 16 => 16,
        d if d <= 24 => 24,
        _ => 32,
    };
    let samples = if out_bd != decoded.bit_depth {
        shift_samples(&samples, decoded.bit_depth, out_bd)
    } else {
        samples
    };

    let bytes = tune_core::audio::alac_encoder::encode_alac_m4a(
        &samples,
        out_bd,
        decoded.channels as u16,
        out_sr,
    )?;
    std::fs::write(output, &bytes).map_err(|e| format!("failed to write {}: {e}", output.display()))
}

/// Re-scale i32 samples from one bit depth to another (values, not bytes —
/// unlike `convert_bit_depth`, which packs bytes for the WAV/FLAC encoders).
fn shift_samples(samples: &[i32], from_bd: u16, to_bd: u16) -> Vec<i32> {
    if from_bd == to_bd {
        return samples.to_vec();
    }
    if to_bd > from_bd {
        let up = to_bd - from_bd;
        samples.iter().map(|&s| s << up).collect()
    } else {
        let down = from_bd - to_bd;
        samples.iter().map(|&s| s >> down).collect()
    }
}

/// Encode to FLAC or WAV using the native Rust pipeline.
fn encode_lossless_native(
    input: &str,
    output: &Path,
    format: &str,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    // Decode to PCM. Only the DSD/WavPack decoders honour target_sr; the
    // symphonia path returns the source rate — the rubato pass below covers it.
    let decoded = decode_for_convert(input, target_sr)?;

    let out_sr = target_sr.unwrap_or(decoded.sample_rate);
    let out_bd = target_bd.unwrap_or(decoded.bit_depth);

    // Resample natively when the decoder didn't (#1525) — this used to be
    // routed to an external ffmpeg that a standard install doesn't have.
    let samples = if out_sr != decoded.sample_rate {
        tune_core::audio::resample::resample_i32(
            &decoded.samples_i32,
            decoded.bit_depth,
            decoded.channels as u16,
            decoded.sample_rate,
            out_sr,
        )
    } else {
        decoded.samples_i32.clone()
    };

    // Convert bit depth if needed.
    let pcm_final = if out_bd == decoded.bit_depth && out_sr == decoded.sample_rate {
        decoded.pcm_bytes()
    } else {
        convert_bit_depth(&samples, decoded.bit_depth, out_bd)
    };

    // Encode
    let encoded = match format {
        "wav" => encode_wav(&pcm_final, out_sr, out_bd as u32, decoded.channels)?,
        "flac" | _ => encode_flac(&pcm_final, out_sr, out_bd as u32, decoded.channels)?,
    };

    std::fs::write(output, &encoded)
        .map_err(|e| format!("failed to write {}: {e}", output.display()))
}

/// Encode to Ogg Opus fully in-process (#1525): native decode → rubato to
/// 48 kHz → libopus (the `opus` crate) → native Ogg mux. Replaces opusenc/ffmpeg.
fn encode_opus_native(input: &str, output: &Path, quality: Option<&str>) -> Result<(), String> {
    let decoded = decode_for_convert(input, None)?;
    if decoded.channels > 2 {
        return Err(format!(
            "opus: {} canaux non pris en charge (mono/stéréo)",
            decoded.channels
        ));
    }

    // Opus is a 48 kHz codec; resample whatever the source rate is.
    let src_sr = decoded.sample_rate;
    let samples = tune_core::audio::resample::resample_i32(
        &decoded.samples_i32,
        decoded.bit_depth,
        decoded.channels as u16,
        src_sr,
        tune_core::audio::opus_ogg::OPUS_SAMPLE_RATE,
    );

    // To i16 for the encoder input.
    let shift = decoded.bit_depth.saturating_sub(16);
    let pcm16: Vec<i16> = samples
        .iter()
        .map(|&s| (s >> shift).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
        .collect();

    // Same quality contract as the old opusenc path: a plain bitrate in kb/s.
    let bitrate_kbps: u32 = quality.and_then(|q| q.parse().ok()).unwrap_or(128);

    let bytes = tune_core::audio::opus_ogg::encode_ogg_opus(
        &pcm16,
        decoded.channels as u16,
        bitrate_kbps,
        src_sr,
    )?;
    std::fs::write(output, &bytes).map_err(|e| format!("failed to write {}: {e}", output.display()))
}

/// Encode PCM bytes to WAV using the existing AudioEncoder from tune-core.
fn encode_wav(
    pcm: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<Vec<u8>, String> {
    let mut encoder =
        tune_core::audio::encoder::AudioEncoder::new("wav", sample_rate, bit_depth, channels);
    let rt = tokio::runtime::Handle::current();
    rt.block_on(async {
        encoder.start().await?;
        encoder.write(pcm).await?;
        encoder.finish().await
    })
}

/// Encode PCM bytes to FLAC using the existing native encoder from tune-core.
fn encode_flac(
    pcm: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<Vec<u8>, String> {
    let mut encoder =
        tune_core::audio::encoder::AudioEncoder::new("flac", sample_rate, bit_depth, channels);

    // The encoder API is async but the internals are CPU-bound and don't
    // actually await anything, so we can use block_on in a blocking context.
    let rt = tokio::runtime::Handle::current();
    rt.block_on(async {
        encoder.start().await?;
        encoder.write(pcm).await?;
        encoder.finish().await
    })
}

/// Convert i32 samples from one bit depth to another, returning PCM bytes.
fn convert_bit_depth(samples: &[i32], from_bd: u16, to_bd: u16) -> Vec<u8> {
    let bytes_per_sample = ((to_bd as usize) + 7) / 8;
    let mut output = Vec::with_capacity(samples.len() * bytes_per_sample);

    for &s in samples {
        let v = match (from_bd, to_bd) {
            (24, 16) => (s >> 8) as i32,
            (32, 16) => (s >> 16) as i32,
            (16, 24) => (s as i32) << 8,
            (32, 24) => s >> 8,
            (16, 32) => (s as i32) << 16,
            (24, 32) => s << 8,
            _ => s,
        };
        match bytes_per_sample {
            2 => output.extend_from_slice(&(v as i16).to_le_bytes()),
            3 => {
                let b = v.to_le_bytes();
                output.extend_from_slice(&b[..3]);
            }
            4 => output.extend_from_slice(&v.to_le_bytes()),
            _ => output.extend_from_slice(&(v as i16).to_le_bytes()),
        }
    }

    output
}

// ---------------------------------------------------------------------------
// External encoder (mp3/aac/alac only — chantier #1523 makes them native;
// flac/wav/opus are fully in-process since #1525)
// ---------------------------------------------------------------------------

/// Try external tools in preference order.  We first try format-specific
/// tools (lame) and fall back to ffmpeg.
async fn encode_with_external(
    input: &str,
    output: &Path,
    format: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    // First, try to decode to a temporary WAV that external tools can read.
    // Many external encoders only accept WAV input.
    let tmp_wav = output.with_extension("_tmp.wav");
    let input_owned = input.to_string();
    let tmp_wav_clone = tmp_wav.clone();
    let sr = target_sr;
    let bd = target_bd;

    tokio::task::spawn_blocking(move || {
        encode_lossless_native(&input_owned, &tmp_wav_clone, "wav", sr, bd)
    })
    .await
    .map_err(|e| format!("decode join error: {e}"))??;

    let tmp_wav_str = tmp_wav
        .to_str()
        .ok_or_else(|| "invalid tmp wav path".to_string())?;
    let output_str = output
        .to_str()
        .ok_or_else(|| "invalid output path".to_string())?;

    let result = match format {
        "mp3" => encode_mp3_external(tmp_wav_str, output_str, quality, target_sr).await,
        "aac" => encode_aac_external(tmp_wav_str, output_str, quality, target_sr).await,
        _ => Err(format!("unsupported external format: {format}")),
    };

    // Clean up tmp WAV
    let _ = tokio::fs::remove_file(&tmp_wav).await;

    result
}

async fn encode_mp3_external(
    input: &str,
    output: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
) -> Result<(), String> {
    // Try lame first
    if let Some(lame) = resolve_tool("lame") {
        let mut args: Vec<String> = Vec::new();

        match quality.unwrap_or("320") {
            "v0" => {
                args.push("-V".into());
                args.push("0".into());
            }
            "v2" => {
                args.push("-V".into());
                args.push("2".into());
            }
            q => {
                args.push("-b".into());
                args.push(q.into());
            }
        }

        if let Some(sr) = target_sr {
            args.push("--resample".into());
            args.push(format!("{}", sr as f64 / 1000.0));
        }

        args.push(input.into());
        args.push(output.into());

        return run_command(&lame, &args).await;
    }

    // Fallback to ffmpeg
    if let Some(ffmpeg) = resolve_tool("ffmpeg") {
        let mut args = vec![
            "-y".to_string(),
            "-i".into(),
            input.into(),
            "-codec:a".into(),
            "libmp3lame".into(),
        ];

        match quality.unwrap_or("320") {
            "v0" => {
                args.push("-q:a".into());
                args.push("0".into());
            }
            "v2" => {
                args.push("-q:a".into());
                args.push("2".into());
            }
            q => {
                args.push("-b:a".into());
                args.push(format!("{q}k"));
            }
        }

        if let Some(sr) = target_sr {
            args.push("-ar".into());
            args.push(sr.to_string());
        }

        args.push(output.into());
        return run_command(&ffmpeg, &args).await;
    }

    Err("mp3 encoding requires lame or ffmpeg (bundled with the release or on PATH)".into())
}

async fn encode_aac_external(
    input: &str,
    output: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
) -> Result<(), String> {
    let bitrate = quality.unwrap_or("256");

    if let Some(ffmpeg) = resolve_tool("ffmpeg") {
        let mut args = vec![
            "-y".to_string(),
            "-i".into(),
            input.into(),
            "-codec:a".into(),
            "aac".into(),
            "-b:a".into(),
            format!("{bitrate}k"),
        ];

        if let Some(sr) = target_sr {
            args.push("-ar".into());
            args.push(sr.to_string());
        }

        args.push(output.into());
        return run_command(&ffmpeg, &args).await;
    }

    Err("aac encoding requires ffmpeg (bundled with the release or on PATH)".into())
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Resolve an external tool to a concrete path (#1524).
///
/// Looks next to the server executable FIRST — that is where the release
/// bundles ffmpeg — then walks PATH in-process. The old implementation
/// shelled out to `which`, which does not exist on Windows: detection
/// always answered false there, even with ffmpeg installed, so aac/alac
/// conversion was reported impossible on every Windows machine.
fn resolve_tool(name: &str) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    // 1. Bundled: same directory as tune-server (release layout).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(&exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // 2. PATH, resolved in-process — no `which`/`where` subprocess.
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(&exe_name))
        .find(|candidate| candidate.is_file())
}

/// Run an external command (resolved to a concrete path) and return an
/// error if it fails.
async fn run_command(program: &Path, args: &[String]) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to run {}: {e}", program.display()))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{} failed (exit {}): {}",
            program.display(),
            output.status.code().unwrap_or(-1),
            stderr.chars().take(500).collect::<String>()
        ))
    }
}

fn output_extension(format: &str) -> &str {
    match format {
        "flac" => "flac",
        "wav" => "wav",
        "mp3" => "mp3",
        "aac" => "m4a",
        "alac" => "m4a",
        "opus" => "opus",
        _ => "bin",
    }
}

/// Recursively collect audio files from a directory.
fn collect_audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_audio_files(&path, out);
        } else if let Some(s) = path.to_str() {
            if convertible_input(s) {
                out.push(path);
            }
        }
    }
}

/// Le convertisseur accepte-t-il ce fichier en ENTRÉE ? Décodage natif, ou
/// WMA/ASF via le ffmpeg résolu du convertisseur (point 12, revue
/// 2026-08-15 : le WMA n'a plus de décodeur natif depuis le retrait de
/// ffmpeg du CHEMIN DE LECTURE en v0.8.46 — la CONVERSION, elle, a le droit
/// au ffmpeg livré avec la release, épic #1523).
fn convertible_input(path: &str) -> bool {
    if can_decode_native(path) {
        return true;
    }
    matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("wma" | "asf")
    ) && resolve_tool("ffmpeg").is_some()
}

/// Décodage d'une entrée du convertisseur : natif quand on sait faire,
/// sinon (WMA/ASF) via le ffmpeg résolu. `target_sr` n'est honoré que par
/// les décodeurs natifs qui le supportent — les appelants rééchantillonnent
/// de toute façon quand `decoded.sample_rate` ne correspond pas.
fn decode_for_convert(
    input: &str,
    target_sr: Option<u32>,
) -> Result<tune_core::audio::decode::DecodedAudio, String> {
    if can_decode_native(input) {
        return decode_to_pcm(input, target_sr, None, 0.0, f64::MAX);
    }
    decode_via_converter_ffmpeg(input)
}

/// Décode WMA/ASF en PCM s24le via le ffmpeg du convertisseur. Le bundle
/// minimal n'a pas ffprobe : la fréquence et les canaux sont lus dans la
/// bannière stderr de `ffmpeg -i`. Si le ffmpeg résolu est le bundle minimal
/// (sans démuxeur ASF), l'échec est propre et nommé.
fn decode_via_converter_ffmpeg(
    input: &str,
) -> Result<tune_core::audio::decode::DecodedAudio, String> {
    let ffmpeg =
        resolve_tool("ffmpeg").ok_or("aucun ffmpeg disponible pour décoder ce format (WMA/ASF)")?;

    let probe = std::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-i", input])
        .output()
        .map_err(|e| format!("ffmpeg probe: {e}"))?;
    let banner = String::from_utf8_lossy(&probe.stderr);
    let (sample_rate, channels) = parse_ffmpeg_audio_banner(&banner).ok_or_else(|| {
        format!(
            "le ffmpeg résolu ne reconnaît pas ce fichier (bundle minimal sans démuxeur ASF ?) : {}",
            banner.lines().last().unwrap_or("").trim()
        )
    })?;

    let out = std::process::Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-i",
            input,
            "-f",
            "s24le",
            "-acodec",
            "pcm_s24le",
            "-",
        ])
        .output()
        .map_err(|e| format!("ffmpeg decode: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffmpeg decode failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let pcm = out.stdout;
    if pcm.is_empty() || pcm.len() % 3 != 0 {
        return Err("ffmpeg decode produced no usable PCM".into());
    }
    let mut samples_i32 = Vec::with_capacity(pcm.len() / 3);
    for b in pcm.chunks_exact(3) {
        let v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
        samples_i32.push((v << 8) >> 8); // sign-extend 24-bit
    }
    let duration_s = samples_i32.len() as f64 / f64::from(channels) / f64::from(sample_rate);
    Ok(tune_core::audio::decode::DecodedAudio {
        samples_i32,
        bit_depth: 24,
        sample_rate,
        channels,
        duration_s,
    })
}

/// Extrait `(sample_rate, channels)` de la ligne « Audio: … » de la bannière
/// stderr de ffmpeg, ex. « Stream #0:0: Audio: wmav2, 44100 Hz, stereo, … ».
fn parse_ffmpeg_audio_banner(stderr: &str) -> Option<(u32, u32)> {
    let line = stderr.lines().find(|l| l.contains("Audio:"))?;
    let mut sample_rate = None;
    let mut channels = None;
    for part in line.split(',') {
        let part = part.trim();
        if let Some(hz) = part.strip_suffix(" Hz") {
            sample_rate = hz.trim().parse::<u32>().ok();
        } else if part == "stereo" {
            channels = Some(2);
        } else if part == "mono" {
            channels = Some(1);
        } else if let Some(n) = part.strip_suffix(" channels") {
            channels = n.trim().parse::<u32>().ok();
        }
    }
    match (sample_rate, channels) {
        (Some(sr), Some(ch)) if sr > 0 && ch > 0 => Some((sr, ch)),
        _ => None,
    }
}

/// Copy metadata tags from source to destination using lofty.
fn copy_tags(source: &Path, dest: &Path) -> Result<(), String> {
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey, TagExt};

    let src_tagged =
        lofty::read_from_path(source).map_err(|e| format!("lofty read source: {e}"))?;

    let src_tag = match src_tagged.primary_tag() {
        Some(t) => t,
        None => return Ok(()), // No tags to copy
    };

    // Read the destination file, attach cloned tags, save
    let mut dst_tagged =
        lofty::read_from_path(dest).map_err(|e| format!("lofty read dest: {e}"))?;

    // Get or create a primary tag on the destination
    let tag_type = dst_tagged.primary_tag().map(|t| t.tag_type());
    let dst_tag = if let Some(tt) = tag_type {
        dst_tagged.tag_mut(tt).ok_or("cannot get dest tag")?
    } else {
        // Insert a new tag of the same type as source
        let tt = src_tag.tag_type();
        dst_tagged.insert_tag(lofty::tag::Tag::new(tt));
        dst_tagged.tag_mut(tt).ok_or("cannot create dest tag")?
    };

    // Copy common fields
    if let Some(v) = src_tag.title() {
        dst_tag.set_title(v.into_owned());
    }
    if let Some(v) = src_tag.artist() {
        dst_tag.set_artist(v.into_owned());
    }
    if let Some(v) = src_tag.album() {
        dst_tag.set_album(v.into_owned());
    }
    if let Some(v) = src_tag.genre() {
        dst_tag.set_genre(v.into_owned());
    }
    if let Some(v) = src_tag.track() {
        dst_tag.set_track(v);
    }
    if let Some(v) = src_tag.disk() {
        dst_tag.set_disk(v);
    }

    // Copy additional items
    for key in [
        ItemKey::Composer,
        ItemKey::Conductor,
        ItemKey::Lyricist,
        ItemKey::Performer,
        ItemKey::Remixer,
        ItemKey::Producer,
        ItemKey::Isrc,
        ItemKey::Label,
        ItemKey::CatalogNumber,
        ItemKey::Barcode,
        ItemKey::Comment,
        ItemKey::AlbumArtist,
        ItemKey::AlbumArtistSortOrder,
        ItemKey::TrackArtistSortOrder,
        ItemKey::AlbumTitleSortOrder,
        ItemKey::Year,
        ItemKey::ReleaseDate,
        ItemKey::OriginalReleaseDate,
        ItemKey::Bpm,
        ItemKey::Mood,
        ItemKey::ContentGroup,
        ItemKey::CopyrightMessage,
        ItemKey::Language,
        ItemKey::EncodedBy,
        ItemKey::FlagCompilation,
        ItemKey::Lyrics,
        ItemKey::MusicBrainzRecordingId,
        ItemKey::MusicBrainzReleaseId,
        ItemKey::MusicBrainzArtistId,
        ItemKey::MusicBrainzReleaseArtistId,
        ItemKey::MusicBrainzReleaseGroupId,
        ItemKey::MusicBrainzWorkId,
        ItemKey::ReplayGainTrackGain,
        ItemKey::ReplayGainTrackPeak,
        ItemKey::ReplayGainAlbumGain,
        ItemKey::ReplayGainAlbumPeak,
    ] {
        if let Some(item) = src_tag.get(key.clone()) {
            dst_tag.push(item.clone());
        }
    }

    // Copy embedded cover art. Pictures are NOT ItemKeys — lofty stores them
    // in a separate list, so the tag-item loop above misses them entirely and
    // the converted file ends up with no cover (Scordia, #999). Copy every
    // source picture across.
    for pic in src_tag.pictures() {
        dst_tag.push_picture(pic.clone());
    }

    dst_tag
        .save_to_path(dest, lofty::config::WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    Ok(())
}

/// Build a ZIP archive from all files in the output directory.
fn build_zip(dir: &Path) -> Result<Vec<u8>, String> {
    use std::io::{Read, Write};

    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zip = zip::ZipWriter::new(cursor);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("read output dir: {e}"))?
            .flatten()
            .filter(|e| e.path().is_file())
            // Skip temporary WAV files used during encoding
            .filter(|e| {
                !e.path()
                    .to_str()
                    .map(|s| s.ends_with("_tmp.wav"))
                    .unwrap_or(false)
            })
            .collect();

        for entry in entries {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

            zip.start_file(name, options)
                .map_err(|e| format!("zip start_file: {e}"))?;

            let mut f =
                std::fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
            let mut data = Vec::new();
            f.read_to_end(&mut data)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            zip.write_all(&data)
                .map_err(|e| format!("zip write: {e}"))?;
        }

        zip.finish().map_err(|e| format!("zip finish: {e}"))?;
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_parsing_extrait_frequence_et_canaux() {
        let banner = "Input #0, asf, from 'x.wma':\n  Duration: 00:03:12.34\n    Stream #0:0: Audio: wmav2 (a[1][0][0] / 0x0161), 44100 Hz, stereo, fltp, 128 kb/s";
        assert_eq!(parse_ffmpeg_audio_banner(banner), Some((44100, 2)));
        let mono = "    Stream #0:0: Audio: wmav2, 22050 Hz, mono, fltp, 64 kb/s";
        assert_eq!(parse_ffmpeg_audio_banner(mono), Some((22050, 1)));
        let multi = "    Stream #0:0: Audio: wmapro, 48000 Hz, 6 channels, fltp";
        assert_eq!(parse_ffmpeg_audio_banner(multi), Some((48000, 6)));
        // Pas de ligne Audio (bundle minimal sans démuxeur ASF) → None.
        assert_eq!(parse_ffmpeg_audio_banner("x.wma: Invalid data found"), None);
    }

    #[test]
    fn ffmpeg_encoders_parsing_reads_the_second_column() {
        // Real `ffmpeg -encoders` shape: legend, separator, then entries.
        let out = "Encoders:\n V..... = Video\n A..... = Audio\n ------\n \
                   A....D aac              AAC (Advanced Audio Coding)\n \
                   A....D alac             ALAC (Apple Lossless Audio Codec)\n \
                   V....D libx264          H.264\n";
        let set = parse_ffmpeg_encoders(out);
        assert!(set.contains("aac"));
        assert!(set.contains("alac"));
        assert!(set.contains("libx264"));
        assert!(
            !set.contains("libmp3lame"),
            "absent encoder must stay absent"
        );
        // The minimal bundled build ships exactly aac+alac: mp3 must NOT be
        // inferred from ffmpeg's mere presence.
    }

    #[test]
    fn resolve_tool_prefers_the_bundled_binary() {
        // A tool named after this test placed next to the current executable
        // must win over PATH. We can't write next to the test runner binary
        // reliably, so assert the negative contract instead: an improbable
        // name resolves to None, and a ubiquitous one resolves to a real file.
        assert!(resolve_tool("tune-no-such-tool-58d2").is_none());
        #[cfg(unix)]
        {
            let sh = resolve_tool("sh").expect("sh must exist on unix PATH");
            assert!(sh.is_file());
        }
    }
}
