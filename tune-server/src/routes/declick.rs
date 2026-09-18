//! Dé-ploc (declick) — PREMIUM batch tool.
//!
//! Modeled on the batch Converter (`converter.rs`) but with a different job:
//! trim the digital silence (and the tell-tale "ploc"/click of a non-zero edge)
//! from the head and tail of each track. The tool is **100% native Rust** — it
//! never shells out to ffmpeg/lame/opusenc (FFmpeg was removed from the project
//! in v0.8.46). Any input Symphonia can decode is accepted; the cleaned output is
//! always **lossless** (FLAC by default, WAV optional), since the only native
//! encoder available (`AudioEncoder`) supports FLAC and WAV. This deliberately
//! never lossy-transcodes.

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

use tune_core::audio::decode::can_decode_native;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub track_id: Option<i64>,
    /// Whole album: expanded to all of its tracks' files (mirrors the Converter,
    /// whose web UI selects albums).
    pub album_id: Option<i64>,
    pub path: Option<String>,
}

/// Declick knobs. All optional with sensible defaults so the web UI can post an
/// empty `{}`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DeclickOptions {
    /// Silence threshold in dBFS. Frames whose every channel sits below this are
    /// considered silence and trimmed. Default -60.0 dB.
    pub threshold_db: Option<f32>,
    /// Trim leading silence. Default true.
    pub trim_lead: Option<bool>,
    /// Trim trailing silence. Default true.
    pub trim_tail: Option<bool>,
    /// Snap the trim edges to the nearest zero crossing (on channel 0) to avoid
    /// introducing a click. Default true.
    pub zero_cross: Option<bool>,
    /// Output container: "flac" (default) or "wav". Any other value falls back
    /// to FLAC. The tool is always lossless.
    pub output_format: Option<String>,
}

/// Resolved options with defaults applied.
#[derive(Debug, Clone, Copy)]
struct ResolvedOptions {
    threshold_db: f32,
    trim_lead: bool,
    trim_tail: bool,
    zero_cross: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartJobRequest {
    pub sources: Vec<Source>,
    #[serde(default)]
    pub options: DeclickOptions,
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
    path: String,
    error: String,
}

struct DeclickJob {
    cancellation: Arc<std::sync::atomic::AtomicBool>,
    status: JobStatus,
    total: usize,
    completed: usize,
    current_file: String,
    errors: Vec<JobError>,
    output_dir: PathBuf,
}

type JobStore = Arc<Mutex<HashMap<String, Arc<Mutex<DeclickJob>>>>>;

/// Per-process job store — its own singleton, independent of the Converter's.
fn job_store() -> JobStore {
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
        .route("/jobs/{job_id}", delete(cancel_job))
}

// ---------------------------------------------------------------------------
// POST /start — kick off a batch declick
// ---------------------------------------------------------------------------

async fn start_job(
    State(state): State<AppState>,
    Json(body): Json<StartJobRequest>,
) -> Result<axum::response::Response, AppError> {
    // Premium gate FIRST.
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::Declick)
            .await
    {
        return Ok(resp);
    }
    if let Err(response) = crate::premium_audio_plugins::require_installed(&state, "declick") {
        return Ok(response);
    }

    // Resolve options + output format.
    let opts = ResolvedOptions {
        threshold_db: body.options.threshold_db.unwrap_or(-60.0),
        trim_lead: body.options.trim_lead.unwrap_or(true),
        trim_tail: body.options.trim_tail.unwrap_or(true),
        zero_cross: body.options.zero_cross.unwrap_or(true),
    };
    let out_format = match body
        .options
        .output_format
        .as_deref()
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("wav") => "wav",
        // "flac" or anything else → lossless FLAC default.
        _ => "flac",
    };

    // Resolve all source paths (identical strategy to the Converter).
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut file_paths: Vec<PathBuf> = Vec::new();

    for src in &body.sources {
        if let Some(track_id) = src.track_id {
            match repo.get(track_id) {
                Ok(Some(track)) => {
                    if let Some(ref fp) = track.file_path {
                        file_paths.push(PathBuf::from(fp));
                    } else {
                        warn!(track_id, "declick_skip_no_file_path");
                    }
                }
                Ok(None) => warn!(track_id, "declick_skip_track_not_found"),
                Err(e) => warn!(track_id, error = %e, "declick_skip_track_lookup_error"),
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
                Err(e) => warn!(album_id, error = %e, "declick_skip_album_lookup_error"),
            }
        } else if let Some(ref path) = src.path {
            let p = PathBuf::from(path);
            if p.is_dir() {
                collect_audio_files(&p, &mut file_paths);
            } else if p.is_file() && can_decode_native(path) {
                file_paths.push(p);
            } else {
                warn!(path, "declick_skip_not_audio_or_missing");
            }
        }
    }

    if file_paths.is_empty() {
        return Err(AppError::bad_request("no audio files found in sources"));
    }

    let total = file_paths.len();
    let job_id = uuid::Uuid::new_v4().to_string();
    let output_dir = PathBuf::from(format!("/tmp/tune-declick/{}", job_id));
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|e| AppError::internal(format!("failed to create output dir: {e}")))?;

    let job = Arc::new(Mutex::new(DeclickJob {
        cancellation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        status: JobStatus::Running,
        total,
        completed: 0,
        current_file: String::new(),
        errors: Vec::new(),
        output_dir: output_dir.clone(),
    }));

    crate::audio_job_journal::write("declick", &job_id, "running", total, 0)
        .map_err(AppError::internal)?;
    let store = job_store();
    {
        let mut map = store.lock().await;
        map.insert(job_id.clone(), job.clone());
    }

    // Spawn the background worker.
    let jid = job_id.clone();
    let journal_job = job.clone();
    tokio::spawn(async move {
        run_declick(job, file_paths, opts, out_format, &output_dir).await;
        let final_job = journal_job.lock().await;
        if let Err(error) = crate::audio_job_journal::write_result(
            "declick",
            &jid,
            &json!({"job_id":jid,"status":final_job.status.as_str(),"total":final_job.total,"completed":final_job.completed,"errors":final_job.errors.iter().map(|e|json!({"path":e.path,"error":e.error})).collect::<Vec<_>>()}),
        ) {
            tracing::error!(%error, "audio_job_status_not_persisted");
        }
        info!(job_id = %jid, "declick_job_finished");
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
    if !map.contains_key(&job_id) {
        if let Some(status) = crate::audio_job_journal::recovered("declick", &job_id) {
            return Ok(Json(status));
        }
    }
    let job_arc = map
        .get(&job_id)
        .ok_or_else(|| AppError::not_found(format!("job not found: {job_id}")))?
        .clone();
    let job = job_arc.lock().await;

    let errors: Vec<Value> = job
        .errors
        .iter()
        .map(|e| json!({"path": e.path, "error": e.error}))
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
// GET /download/{job_id} — stream a ZIP of the cleaned files
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

    let zip_bytes = tokio::task::spawn_blocking(move || build_zip(&output_dir))
        .await
        .map_err(|e| AppError::internal(format!("zip task join error: {e}")))?
        .map_err(|e| AppError::internal(format!("zip build error: {e}")))?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/zip"));
    headers.insert(
        "Content-Disposition",
        HeaderValue::from_str(&format!(
            "attachment; filename=\"tune-declick-{job_id}.zip\""
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"declick.zip\"")),
    );

    Ok((StatusCode::OK, headers, Body::from(zip_bytes)))
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
            job.cancellation
                .store(true, std::sync::atomic::Ordering::Release);
            job.status = JobStatus::Cancelled;
        }
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
// Background declick worker
// ---------------------------------------------------------------------------

async fn run_declick(
    job: Arc<Mutex<DeclickJob>>,
    files: Vec<PathBuf>,
    opts: ResolvedOptions,
    out_format: &'static str,
    output_dir: &Path,
) {
    for file_path in &files {
        // Cancellation check.
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

        let out_path = output_dir.join(format!("{filename}.{out_format}"));

        let input_owned = file_path.clone();
        let output_owned = out_path.clone();
        let cancellation = job.lock().await.cancellation.clone();
        let result = tokio::task::spawn_blocking(move || {
            process_single_file(&input_owned, &output_owned, opts, out_format, cancellation)
        })
        .await
        .unwrap_or_else(|e| Err(format!("spawn_blocking join error: {e}")));

        match result {
            Ok(()) => {
                let mut j = job.lock().await;
                j.completed += 1;
            }
            Err(e) => {
                error!(file = %file_path.display(), error = %e, "declick_file_failed");
                let mut j = job.lock().await;
                j.completed += 1;
                j.errors.push(JobError {
                    path: file_path.display().to_string(),
                    error: e,
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
// Engine — decode → trim → encode (100% native)
// ---------------------------------------------------------------------------

/// Decode `input`, trim leading/trailing silence (optionally snapping to zero
/// crossings), then re-encode the cleaned audio to lossless FLAC or WAV.
fn process_single_file(
    input: &Path,
    output: &Path,
    opts: ResolvedOptions,
    out_format: &str,
    cancellation: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    super::premium_audio_host::run_installed(
        "declick",
        &tune_plugin_declick::Declick,
        input,
        output,
        &json!({"threshold_db": opts.threshold_db, "trim_lead": opts.trim_lead, "trim_tail": opts.trim_tail, "zero_cross": opts.zero_cross, "output_format": out_format}),
        cancellation,
    )
}

// ---------------------------------------------------------------------------
// Helpers (duplicated from converter.rs — those are private there)
// ---------------------------------------------------------------------------

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

/// Build a ZIP archive (Stored, no compression) from all files in `dir`.
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
