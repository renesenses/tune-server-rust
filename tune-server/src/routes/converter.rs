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
        .route("/jobs/{job_id}", delete(cancel_job))
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
        } else if let Some(ref path) = src.path {
            let p = PathBuf::from(path);
            if p.is_dir() {
                collect_audio_files(&p, &mut file_paths);
            } else if p.is_file() && can_decode_native(path) {
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

    // For lossy formats (mp3, aac, opus) and alac, shell out to external tools
    // since the native Rust encoder only supports WAV and FLAC.
    // Also use the external path when sample rate conversion is requested for
    // lossless formats — the native Symphonia decode path does not resample,
    // so we let ffmpeg handle the full pipeline in that case.
    let needs_external = match format {
        "mp3" | "aac" | "opus" | "alac" => true,
        "flac" | "wav" if target_sr.is_some() => true,
        _ => false,
    };
    if needs_external {
        return encode_with_external(input_str, output, format, quality, target_sr, target_bd)
            .await;
    }

    // Lossless formats (FLAC, WAV): use native Rust decoders + encoder
    let input_owned = input_str.to_string();
    let format_owned = format.to_string();
    let sr = target_sr;
    let bd = target_bd;
    let output_owned = output.to_path_buf();

    tokio::task::spawn_blocking(move || {
        encode_lossless_native(&input_owned, &output_owned, &format_owned, sr, bd)
    })
    .await
    .map_err(|e| format!("spawn_blocking join error: {e}"))?
}

/// Encode to FLAC or WAV using the native Rust pipeline.
fn encode_lossless_native(
    input: &str,
    output: &Path,
    format: &str,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    // Decode to PCM
    let decoded = decode_to_pcm(input, target_sr, None, 0.0, f64::MAX)?;

    let out_sr = target_sr.unwrap_or(decoded.sample_rate);
    let out_bd = target_bd.unwrap_or(decoded.bit_depth);

    // decode_to_pcm already handled resampling if target_sr was set.
    // Convert bit depth if needed.
    let pcm_final = if out_bd == decoded.bit_depth {
        decoded.pcm_bytes()
    } else {
        convert_bit_depth(&decoded.samples_i32, decoded.bit_depth, out_bd)
    };

    // Encode
    let encoded = match format {
        "wav" => encode_wav(&pcm_final, out_sr, out_bd as u32, decoded.channels)?,
        "flac" | _ => encode_flac(&pcm_final, out_sr, out_bd as u32, decoded.channels)?,
    };

    std::fs::write(output, &encoded)
        .map_err(|e| format!("failed to write {}: {e}", output.display()))
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
// External encoder (ffmpeg / lame / opusenc fallback)
// ---------------------------------------------------------------------------

/// Try external tools in preference order.  We first try format-specific
/// tools (lame, opusenc, fdkaac) and fall back to ffmpeg.
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
        "opus" => encode_opus_external(tmp_wav_str, output_str, quality).await,
        "aac" => encode_aac_external(tmp_wav_str, output_str, quality, target_sr).await,
        "alac" => encode_alac_external(tmp_wav_str, output_str, target_sr).await,
        "flac" => {
            encode_flac_external(tmp_wav_str, output_str, quality, target_sr, target_bd).await
        }
        "wav" => encode_wav_external(tmp_wav_str, output_str, target_sr, target_bd).await,
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
    if tool_available("lame").await {
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

        return run_command("lame", &args).await;
    }

    // Fallback to ffmpeg
    if tool_available("ffmpeg").await {
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
        return run_command("ffmpeg", &args).await;
    }

    Err("mp3 encoding requires lame or ffmpeg on PATH".into())
}

async fn encode_opus_external(
    input: &str,
    output: &str,
    quality: Option<&str>,
) -> Result<(), String> {
    let bitrate = quality.unwrap_or("128");

    // Try opusenc first
    if tool_available("opusenc").await {
        let args = vec![
            "--bitrate".to_string(),
            bitrate.into(),
            input.into(),
            output.into(),
        ];
        return run_command("opusenc", &args).await;
    }

    // Fallback to ffmpeg
    if tool_available("ffmpeg").await {
        let args = vec![
            "-y".to_string(),
            "-i".into(),
            input.into(),
            "-codec:a".into(),
            "libopus".into(),
            "-b:a".into(),
            format!("{bitrate}k"),
            output.into(),
        ];
        return run_command("ffmpeg", &args).await;
    }

    Err("opus encoding requires opusenc or ffmpeg on PATH".into())
}

async fn encode_aac_external(
    input: &str,
    output: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
) -> Result<(), String> {
    let bitrate = quality.unwrap_or("256");

    if tool_available("ffmpeg").await {
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
        return run_command("ffmpeg", &args).await;
    }

    Err("aac encoding requires ffmpeg on PATH".into())
}

async fn encode_alac_external(
    input: &str,
    output: &str,
    target_sr: Option<u32>,
) -> Result<(), String> {
    if tool_available("ffmpeg").await {
        let mut args = vec![
            "-y".to_string(),
            "-i".into(),
            input.into(),
            "-codec:a".into(),
            "alac".into(),
        ];

        if let Some(sr) = target_sr {
            args.push("-ar".into());
            args.push(sr.to_string());
        }

        args.push(output.into());
        return run_command("ffmpeg", &args).await;
    }

    Err("alac encoding requires ffmpeg on PATH".into())
}

async fn encode_flac_external(
    input: &str,
    output: &str,
    quality: Option<&str>,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    if tool_available("ffmpeg").await {
        let mut args = vec![
            "-y".to_string(),
            "-i".into(),
            input.into(),
            "-codec:a".into(),
            "flac".into(),
        ];

        // FLAC compression level (0-8)
        let level = quality.unwrap_or("5");
        args.push("-compression_level".into());
        args.push(level.into());

        if let Some(sr) = target_sr {
            args.push("-ar".into());
            args.push(sr.to_string());
        }
        if let Some(bd) = target_bd {
            args.push("-sample_fmt".into());
            args.push(match bd {
                16 => "s16".into(),
                24 => "s32".into(), // ffmpeg FLAC uses s32 for 24-bit
                _ => format!("s{bd}"),
            });
        }

        args.push(output.into());
        return run_command("ffmpeg", &args).await;
    }

    Err("flac encoding with resampling requires ffmpeg on PATH".into())
}

async fn encode_wav_external(
    input: &str,
    output: &str,
    target_sr: Option<u32>,
    target_bd: Option<u16>,
) -> Result<(), String> {
    if tool_available("ffmpeg").await {
        let mut args = vec![
            "-y".to_string(),
            "-i".into(),
            input.into(),
            "-codec:a".into(),
            "pcm_s16le".into(),
        ];

        if let Some(bd) = target_bd {
            // Replace the codec with the right PCM format
            args[4] = match bd {
                16 => "pcm_s16le".into(),
                24 => "pcm_s24le".into(),
                32 => "pcm_s32le".into(),
                _ => "pcm_s16le".into(),
            };
        }

        if let Some(sr) = target_sr {
            args.push("-ar".into());
            args.push(sr.to_string());
        }

        args.push(output.into());
        return run_command("ffmpeg", &args).await;
    }

    Err("wav encoding with resampling requires ffmpeg on PATH".into())
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Check whether a command-line tool is available on PATH.
async fn tool_available(name: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run an external command and return an error if it fails.
async fn run_command(program: &str, args: &[String]) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to run {program}: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{program} failed (exit {}): {}",
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
            if can_decode_native(s) {
                out.push(path);
            }
        }
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

    // Copy additional items (composer, ISRC, year, etc.)
    for key in [
        ItemKey::Composer,
        ItemKey::Isrc,
        ItemKey::Label,
        ItemKey::Comment,
        ItemKey::AlbumArtist,
        ItemKey::Year,
        ItemKey::MusicBrainzRecordingId,
        ItemKey::MusicBrainzReleaseId,
    ] {
        if let Some(item) = src_tag.get(key.clone()) {
            dst_tag.push(item.clone());
        }
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
