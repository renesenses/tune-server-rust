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

use tune_core::audio::decode::{can_decode_native, decode_to_pcm};
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

    // Spawn the background worker.
    let jid = job_id.clone();
    tokio::spawn(async move {
        run_declick(job, file_paths, opts, out_format, &output_dir).await;
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
        let result = tokio::task::spawn_blocking(move || {
            process_single_file(&input_owned, &output_owned, opts, out_format)
        })
        .await
        .unwrap_or_else(|e| Err(format!("spawn_blocking join error: {e}")));

        match result {
            Ok(()) => {
                // Carry the source tags across to the cleaned file.
                if let Err(e) = copy_tags(file_path, &out_path) {
                    warn!(
                        src = %file_path.display(),
                        dst = %out_path.display(),
                        error = %e,
                        "declick_copy_tags_failed"
                    );
                }
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
) -> Result<(), String> {
    let input_str = input
        .to_str()
        .ok_or_else(|| "invalid input path".to_string())?;

    // Native decode of any supported input to interleaved i32 PCM.
    let decoded = decode_to_pcm(input_str, None, None, 0.0, f64::MAX)?;

    let channels = decoded.channels.max(1) as usize;
    let bit_depth = decoded.bit_depth;
    let sample_rate = decoded.sample_rate;
    let samples = &decoded.samples_i32;

    if samples.is_empty() {
        return Err("decoded audio is empty".to_string());
    }

    let total_frames = samples.len() / channels;
    if total_frames == 0 {
        return Err("decoded audio has no complete frames".to_string());
    }

    // samples_i32 are RIGHT-JUSTIFIED at `bit_depth` (a 16-bit sample lives in
    // bits 0..15, 24-bit in 0..23), so digital full scale is 2^(bit_depth-1).
    // Linear silence threshold amplitude = 10^(dB/20) * full_scale.
    let full_scale = (1i64 << (bit_depth.saturating_sub(1)).max(1)) as f64;
    let threshold_lin = 10f64.powf(opts.threshold_db as f64 / 20.0) * full_scale;

    // A frame is "loud" if ANY channel exceeds the threshold.
    let frame_is_loud = |f: usize| -> bool {
        let base = f * channels;
        for c in 0..channels {
            if (samples[base + c].unsigned_abs() as f64) > threshold_lin {
                return true;
            }
        }
        false
    };

    // Leading edge.
    let mut lead_start = 0usize;
    if opts.trim_lead {
        match (0..total_frames).find(|&f| frame_is_loud(f)) {
            Some(f) => lead_start = f,
            None => {
                // Whole track is below threshold: nothing meaningful to keep.
                return Err("track is entirely below the silence threshold".to_string());
            }
        }
        if opts.zero_cross && lead_start > 0 {
            lead_start = snap_zero_crossing_back(samples, channels, lead_start);
        }
    }

    // Trailing edge (inclusive frame index of the last kept frame).
    let mut tail_end = total_frames - 1;
    if opts.trim_tail {
        match (0..total_frames).rev().find(|&f| frame_is_loud(f)) {
            Some(f) => tail_end = f,
            None => {
                return Err("track is entirely below the silence threshold".to_string());
            }
        }
        if opts.zero_cross && tail_end + 1 < total_frames {
            tail_end = snap_zero_crossing_fwd(samples, channels, tail_end, total_frames);
        }
    }

    // Validate the window before slicing — never emit empty/garbage output.
    if lead_start > tail_end {
        return Err(format!(
            "invalid trim window (lead_start {lead_start} > tail_end {tail_end})"
        ));
    }

    let slice = &samples[lead_start * channels..(tail_end + 1) * channels];
    if slice.is_empty() {
        return Err("trimmed audio is empty".to_string());
    }

    // Reuse DecodedAudio::pcm_bytes() to serialize the trimmed slice at its
    // native bit depth, then hand it to the native encoder.
    let trimmed = tune_core::audio::decode::DecodedAudio {
        samples_i32: slice.to_vec(),
        bit_depth,
        sample_rate,
        channels: channels as u32,
        duration_s: slice.len() as f64 / channels as f64 / sample_rate.max(1) as f64,
    };
    let pcm = trimmed.pcm_bytes();

    let encoded = encode_native(
        &pcm,
        sample_rate,
        bit_depth as u32,
        channels as u32,
        out_format,
    )?;

    std::fs::write(output, &encoded)
        .map_err(|e| format!("failed to write {}: {e}", output.display()))
}

/// Move a leading edge earlier to the nearest zero crossing on channel 0, so the
/// cleaned file starts on a zero-valued sample rather than a step (the "ploc").
/// Searches back up to ~50 ms; returns the original index if none is found.
fn snap_zero_crossing_back(samples: &[i32], channels: usize, start: usize) -> usize {
    let window = start; // scan the whole lead-in silence; it's short by construction
    let ch0 = |f: usize| samples[f * channels];
    let mut f = start;
    let lo = start.saturating_sub(window);
    while f > lo {
        let cur = ch0(f);
        let prev = ch0(f - 1);
        if cur == 0 {
            return f;
        }
        // Sign change between prev and cur → crossing sits at f.
        if (prev <= 0 && cur >= 0) || (prev >= 0 && cur <= 0) {
            return f;
        }
        f -= 1;
    }
    start
}

/// Extend a trailing edge later to the nearest zero crossing on channel 0, so the
/// cleaned file ends on a zero-valued sample rather than a step.
fn snap_zero_crossing_fwd(
    samples: &[i32],
    channels: usize,
    end: usize,
    total_frames: usize,
) -> usize {
    let ch0 = |f: usize| samples[f * channels];
    let mut f = end;
    while f + 1 < total_frames {
        let cur = ch0(f);
        let next = ch0(f + 1);
        if cur == 0 {
            return f;
        }
        if (cur <= 0 && next >= 0) || (cur >= 0 && next <= 0) {
            return f;
        }
        f += 1;
    }
    end
}

/// Encode PCM bytes to FLAC or WAV using the native `AudioEncoder`.
fn encode_native(
    pcm: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    format: &str,
) -> Result<Vec<u8>, String> {
    let fmt = if format == "wav" { "wav" } else { "flac" };
    let mut encoder =
        tune_core::audio::encoder::AudioEncoder::new(fmt, sample_rate, bit_depth, channels);

    // The encoder API is async but CPU-bound internally; we're already on a
    // blocking thread (spawn_blocking) with a live Tokio handle.
    let rt = tokio::runtime::Handle::current();
    rt.block_on(async {
        encoder.start().await?;
        encoder.write(pcm).await?;
        encoder.finish().await
    })
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

/// Copy metadata tags (and cover art) from source to destination using lofty.
fn copy_tags(source: &Path, dest: &Path) -> Result<(), String> {
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey, TagExt};

    let src_tagged =
        lofty::read_from_path(source).map_err(|e| format!("lofty read source: {e}"))?;

    let src_tag = match src_tagged.primary_tag() {
        Some(t) => t,
        None => return Ok(()),
    };

    let mut dst_tagged =
        lofty::read_from_path(dest).map_err(|e| format!("lofty read dest: {e}"))?;

    let tag_type = dst_tagged.primary_tag().map(|t| t.tag_type());
    let dst_tag = if let Some(tt) = tag_type {
        dst_tagged.tag_mut(tt).ok_or("cannot get dest tag")?
    } else {
        let tt = src_tag.tag_type();
        dst_tagged.insert_tag(lofty::tag::Tag::new(tt));
        dst_tagged.tag_mut(tt).ok_or("cannot create dest tag")?
    };

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

    for pic in src_tag.pictures() {
        dst_tag.push_picture(pic.clone());
    }

    dst_tag
        .save_to_path(dest, lofty::config::WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    Ok(())
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
