//! `/api/v1/library/ingest` — put freshly-acquired files into the library.
//!
//! The flow is deliberately three-legged so nothing moves before the user has
//! seen it:
//!
//! 1. `analyze` — read the source folder's tags, guess the album, flag holes,
//!    optionally ask MusicBrainz what this release is.
//! 2. `plan` — render the destination paths and surface every conflict.
//! 3. `apply` — move or copy the files, write back any corrected album fields,
//!    then run a targeted scan so the album shows up in the library.
//!
//! `apply` runs in the background and records an undo manifest, because a copy
//! onto a NAS can outlast any sensible HTTP timeout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::ingest::{
    self, AlbumOverrides, AlbumSummary, ConflictPolicy, FileMode, IngestPlan, IngestReport,
    SourceTrack, TrackOverride,
};

use crate::error::AppError;
use crate::state::AppState;

/// Newest-first cap on the job history we keep in `settings`.
const MAX_JOB_HISTORY: usize = 25;

/// How many candidate releases to offer. Enough to cover the usual
/// standard / deluxe / regional spread without turning the step into a list to
/// scroll through.
const MAX_RELEASE_CANDIDATES: usize = 8;

const KEY_MODE: &str = "ingest_file_mode";
const KEY_TEMPLATE: &str = "ingest_template";
const KEY_DEST_ROOT: &str = "ingest_dest_root";
const KEY_CONFLICT: &str = "ingest_conflict_policy";
const KEY_WRITE_TAGS: &str = "ingest_write_tags";
const KEY_JOB_INDEX: &str = "ingest_jobs";

// -- Settings --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct IngestSettings {
    /// Default action on the source files, overridable per import.
    pub mode: FileMode,
    pub template: String,
    /// Music directory new albums land in. `None` until chosen; the first
    /// configured music directory is used as the fallback.
    pub dest_root: Option<String>,
    pub conflict_policy: ConflictPolicy,
    /// Write corrected album fields back into the files after placing them.
    pub write_tags: bool,
}

impl Default for IngestSettings {
    fn default() -> Self {
        Self {
            mode: FileMode::Move,
            template: ingest::DEFAULT_TEMPLATE.to_string(),
            dest_root: None,
            conflict_policy: ConflictPolicy::Skip,
            write_tags: true,
        }
    }
}

fn load_settings(state: &AppState) -> IngestSettings {
    let repo = SettingsRepo::with_backend(state.backend.clone());
    let get = |key: &str| repo.get(key).ok().flatten();

    let mut s = IngestSettings::default();
    if let Some(v) = get(KEY_MODE).and_then(|v| FileMode::parse(&v)) {
        s.mode = v;
    }
    if let Some(v) = get(KEY_TEMPLATE).filter(|v| !v.trim().is_empty()) {
        s.template = v;
    }
    s.dest_root = get(KEY_DEST_ROOT).filter(|v| !v.trim().is_empty());
    if let Some(v) = get(KEY_CONFLICT) {
        s.conflict_policy = match v.as_str() {
            "overwrite" => ConflictPolicy::Overwrite,
            "rename" => ConflictPolicy::Rename,
            _ => ConflictPolicy::Skip,
        };
    }
    if let Some(v) = get(KEY_WRITE_TAGS) {
        s.write_tags = v != "false";
    }
    s
}

/// Music directories, mirroring the fallback used by `/system/config`: the
/// stored setting wins, else whatever the server was started with.
fn music_dirs(state: &AppState) -> Vec<String> {
    let stored: Option<Vec<String>> = SettingsRepo::with_backend(state.backend.clone())
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    match stored {
        Some(dirs) if !dirs.is_empty() => dirs,
        _ => state.config.music_dirs.clone(),
    }
}

/// Resolve the destination root for an import: explicit request value, else the
/// configured default, else the first music directory.
fn resolve_dest_root(
    state: &AppState,
    requested: Option<&str>,
    settings: &IngestSettings,
) -> Result<String, AppError> {
    let candidate = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| settings.dest_root.clone())
        .or_else(|| music_dirs(state).into_iter().next());

    let root = candidate.ok_or_else(|| {
        AppError::bad_request(
            "no destination: configure a music directory first, or pass dest_root",
        )
    })?;
    let root = tune_core::scanner::walker::normalize_path(&root);

    // Refuse to write outside the library: the destination has to be a
    // configured music directory (or inside one), otherwise the album lands
    // somewhere the scanner will never look.
    let dirs = music_dirs(state);
    // Même défaut de séparateur que dans `scan.rs` : `{d}/` code `/` en dur,
    // donc sous Windows AUCUNE destination n'était jamais jugée « dedans » et
    // tout import était refusé. Constaté sur .42 (`D:\data\music`).
    let inside = dirs.iter().any(|d| {
        let d = tune_core::scanner::walker::normalize_path(d);
        crate::routes::system::scan::sous_le_dossier(&root, &d)
    });
    if !dirs.is_empty() && !inside {
        return Err(AppError::bad_request(format!(
            "destination {root} is outside the configured music directories"
        )));
    }

    Ok(root)
}

pub(super) async fn get_ingest_settings(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let settings = load_settings(&state);
    let dirs = music_dirs(&state);
    let effective_root = settings.dest_root.clone().or_else(|| dirs.first().cloned());

    Ok(Json(json!({
        "mode": settings.mode,
        "template": settings.template,
        "default_template": ingest::DEFAULT_TEMPLATE,
        "dest_root": settings.dest_root,
        "effective_dest_root": effective_root,
        "music_dirs": dirs,
        "conflict_policy": settings.conflict_policy,
        "write_tags": settings.write_tags,
    })))
}

#[derive(Deserialize)]
pub(super) struct UpdateSettingsBody {
    mode: Option<String>,
    template: Option<String>,
    dest_root: Option<String>,
    conflict_policy: Option<String>,
    write_tags: Option<bool>,
}

pub(super) async fn put_ingest_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateSettingsBody>,
) -> Result<impl IntoResponse, AppError> {
    let repo = SettingsRepo::with_backend(state.backend.clone());

    if let Some(ref mode) = body.mode {
        let parsed = FileMode::parse(mode)
            .ok_or_else(|| AppError::bad_request("mode must be 'move' or 'copy'"))?;
        repo.set(KEY_MODE, parsed.as_str())
            .map_err(AppError::internal)?;
    }
    if let Some(ref template) = body.template {
        // An empty template is how the UI asks for the default back.
        let value = if template.trim().is_empty() {
            ingest::DEFAULT_TEMPLATE
        } else {
            template.trim()
        };
        repo.set(KEY_TEMPLATE, value).map_err(AppError::internal)?;
    }
    if let Some(ref root) = body.dest_root {
        let value = if root.trim().is_empty() {
            String::new()
        } else {
            // Validate now, so a bad default cannot break every later import.
            let settings = load_settings(&state);
            resolve_dest_root(&state, Some(root), &settings)?
        };
        repo.set(KEY_DEST_ROOT, &value)
            .map_err(AppError::internal)?;
    }
    if let Some(ref policy) = body.conflict_policy {
        let value = match policy.trim() {
            "skip" | "overwrite" | "rename" => policy.trim(),
            _ => {
                return Err(AppError::bad_request(
                    "conflict_policy must be 'skip', 'overwrite' or 'rename'",
                ));
            }
        };
        repo.set(KEY_CONFLICT, value).map_err(AppError::internal)?;
    }
    if let Some(write_tags) = body.write_tags {
        repo.set(KEY_WRITE_TAGS, if write_tags { "true" } else { "false" })
            .map_err(AppError::internal)?;
    }

    get_ingest_settings(State(state)).await
}

// -- Analyze --

#[derive(Deserialize)]
pub(super) struct AnalyzeBody {
    source_path: String,
    /// Ask MusicBrainz to identify the release (one rate-limited HTTP call).
    identify: Option<bool>,
}

/// Read the source folder: audio files with their tags, plus the sidecar files
/// (cover, cue, log) worth carrying along.
async fn read_source(source: &str) -> Result<(Vec<SourceTrack>, Vec<String>), AppError> {
    let dir = tune_core::scanner::walker::normalize_path(source);
    let path = PathBuf::from(&dir);

    if !path.exists() {
        return Err(AppError::bad_request(format!("{dir} does not exist")));
    }
    if !path.is_dir() {
        return Err(AppError::bad_request(format!("{dir} is not a directory")));
    }

    let scan_dir = dir.clone();
    tokio::task::spawn_blocking(move || {
        let found = tune_core::scanner::walker::list_audio_files(std::slice::from_ref(&scan_dir));

        let mut tracks: Vec<SourceTrack> = found
            .files
            .iter()
            .map(|file| {
                let meta = tune_core::metadata::read_metadata(file);
                let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
                let ext = file
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                let mut track = SourceTrack {
                    source_path: file.to_string_lossy().to_string(),
                    ext,
                    file_size: size,
                    ..Default::default()
                };
                if let Some(m) = meta {
                    track.title = m.title;
                    track.artist = m.artist;
                    track.album_artist = m.album_artist;
                    track.album = m.album;
                    track.year = m.year;
                    track.genre = m.genre;
                    track.track_number = m.track_number;
                    track.disc_number = m.disc_number;
                    track.duration_ms = m.duration_ms;
                    track.format = m.format;
                    track.has_cover = m.has_cover;
                }
                track
            })
            .collect();

        // Stable, human order: disc, then track, then path.
        tracks.sort_by(|a, b| {
            a.disc_number
                .unwrap_or(1)
                .cmp(&b.disc_number.unwrap_or(1))
                .then(
                    a.track_number
                        .unwrap_or(0)
                        .cmp(&b.track_number.unwrap_or(0)),
                )
                .then(a.source_path.cmp(&b.source_path))
        });

        // Sidecars live wherever the audio does: a dropped or picked folder is
        // often a wrapper (`.../Muse - Absolution/*.flac`), so looking only at
        // the chosen folder would miss the cover sitting next to the tracks.
        // Scan the source root plus every directory that actually holds audio,
        // one level each — never a full recursive sweep, which would drag in
        // unrelated images from a parent folder full of downloads.
        let mut extra_dirs: Vec<PathBuf> = vec![PathBuf::from(&scan_dir)];
        for track in &tracks {
            if let Some(parent) = Path::new(&track.source_path).parent() {
                let parent = parent.to_path_buf();
                if !extra_dirs.contains(&parent) {
                    extra_dirs.push(parent);
                }
            }
        }

        let mut extras: Vec<String> = Vec::new();
        for dir in &extra_dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file()
                    && ingest::is_extra_file(&p)
                    && !p
                        .file_name()
                        .map(|n| n.to_string_lossy().starts_with("._"))
                        .unwrap_or(false)
                {
                    extras.push(p.to_string_lossy().to_string());
                }
            }
        }
        extras.sort();
        extras.dedup();

        (tracks, extras)
    })
    .await
    .map_err(|e| AppError::internal(format!("read source: {e}")))
}

pub(super) async fn analyze(
    State(state): State<AppState>,
    Json(body): Json<AnalyzeBody>,
) -> Result<impl IntoResponse, AppError> {
    let (tracks, extras) = read_source(&body.source_path).await?;
    let album = ingest::summarize(&tracks);

    // Warn about re-importing something already in the library — a very common
    // mistake when a download sits in a folder you forgot you already ingested.
    let already_known = count_known_paths(&state, &tracks);

    // Candidate releases, for the user to pick the right edition. Searched on
    // title and artist only: the album artist may be missing from the tags, and
    // an album title alone is usually enough to get the shortlist.
    let mut candidates: Vec<tune_core::metadata::musicbrainz_release::MBReleaseMatch> = Vec::new();
    if body.identify.unwrap_or(false)
        && let Some(title) = album.album.as_deref()
    {
        candidates = tune_core::metadata::musicbrainz_release::lookup_release_candidates(
            title,
            album.album_artist.as_deref().unwrap_or(""),
            Some(album.track_count as u32),
            MAX_RELEASE_CANDIDATES,
        )
        .await;
    }

    let settings = load_settings(&state);
    Ok(Json(json!({
        "source_path": tune_core::scanner::walker::normalize_path(&body.source_path),
        "album": album,
        "tracks": tracks,
        "extras": extras,
        "already_in_library": already_known,
        "musicbrainz_candidates": candidates,
        "defaults": {
            "mode": settings.mode,
            "template": settings.template,
            "conflict_policy": settings.conflict_policy,
            "write_tags": settings.write_tags,
        },
    })))
}

// -- Release track listing --

#[derive(Deserialize)]
pub(super) struct ReleaseTracksBody {
    source_path: String,
    /// MusicBrainz release id, as returned in `musicbrainz_candidates`.
    release_id: String,
}

/// Fetch a chosen release's track listing and propose how it maps onto the
/// files on disk.
///
/// The pairing is decided server-side and returned as before/after pairs: the
/// client shows the difference and sends back only what the user accepted, as
/// `track_overrides` on `plan`/`apply`. Nothing is written here.
pub(super) async fn release_tracks(
    Json(body): Json<ReleaseTracksBody>,
) -> Result<impl IntoResponse, AppError> {
    let (tracks, _extras) = read_source(&body.source_path).await?;
    if tracks.is_empty() {
        return Err(AppError::bad_request(
            "no supported audio files found in that folder",
        ));
    }

    let detail = tune_core::metadata::musicbrainz_release::lookup_release_detail(&body.release_id)
        .await
        .ok_or_else(|| {
            AppError::not_found(format!(
                "MusicBrainz has no release {} (or it could not be reached)",
                body.release_id
            ))
        })?;

    let release: Vec<ingest::ReleaseTrack> = detail
        .tracks
        .iter()
        .map(|t| ingest::ReleaseTrack {
            disc: t.disc,
            position: t.position,
            title: t.title.clone(),
        })
        .collect();

    let proposals = ingest::match_release_tracks(&tracks, &release);
    let changed = proposals.iter().filter(|p| p.changes_anything()).count();
    let unmatched = proposals.iter().filter(|p| !p.matched).count();

    Ok(Json(json!({
        "release": {
            "release_id": detail.release_id,
            "title": detail.title,
            "artist": detail.artist,
            "date": detail.date,
            "year": detail.year,
            "country": detail.country,
            "label": detail.label,
            "catalog_number": detail.catalog_number,
            "disc_count": detail.disc_count,
            "track_count": detail.tracks.len(),
        },
        "tracks": detail.tracks,
        "proposals": proposals,
        "changed": changed,
        "unmatched": unmatched,
    })))
}

/// How many of these source files the library already knows by path.
fn count_known_paths(state: &AppState, tracks: &[SourceTrack]) -> usize {
    use tune_core::db::backend::ToSqlValue;
    tracks
        .iter()
        .filter(|t| {
            state
                .backend
                .query_one(
                    "SELECT 1 FROM tracks WHERE path = $1",
                    &[&t.source_path as &dyn ToSqlValue],
                )
                .ok()
                .flatten()
                .is_some()
        })
        .count()
}

// -- Plan --

/// Everything that decides where the files land. Shared by `plan` and `apply`
/// so the preview the user approved is byte-for-byte what gets executed.
#[derive(Deserialize, Default)]
pub(super) struct IngestParams {
    source_path: String,
    dest_root: Option<String>,
    template: Option<String>,
    mode: Option<String>,
    /// Album-wide corrections.
    #[serde(default)]
    overrides: AlbumOverrides,
    /// Per-file corrections, as accepted from a chosen release's listing.
    /// These feed the destination paths too, not just the tags — a title fixed
    /// here is the title in the filename.
    #[serde(default)]
    track_overrides: Vec<TrackOverride>,
}

/// The plan, the resolved album, and the source tracks *after* corrections —
/// the last one is what `apply` needs to write per-file tags.
struct Built {
    plan: IngestPlan,
    album: AlbumSummary,
    tracks: Vec<SourceTrack>,
}

async fn build(state: &AppState, params: &IngestParams) -> Result<Built, AppError> {
    let settings = load_settings(state);
    let root = resolve_dest_root(state, params.dest_root.as_deref(), &settings)?;

    let (mut tracks, extras) = read_source(&params.source_path).await?;
    if tracks.is_empty() {
        return Err(AppError::bad_request(
            "no supported audio files found in that folder",
        ));
    }

    // Per-file corrections first: the album summary is derived from the tracks,
    // so applying them afterwards would summarise stale titles and numbers.
    ingest::apply_track_overrides(&mut tracks, &params.track_overrides);

    let album = ingest::apply_overrides(&ingest::summarize(&tracks), &params.overrides);
    let template = params
        .template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&settings.template)
        .to_string();
    let mode = params
        .mode
        .as_deref()
        .and_then(FileMode::parse)
        .unwrap_or(settings.mode);

    let plan = ingest::build_plan(
        &tune_core::scanner::walker::normalize_path(&params.source_path),
        &tracks,
        &extras,
        &album,
        &ingest::PlanOptions::new(&root, &template, mode),
        &|p: &Path| p.exists(),
    );

    Ok(Built {
        plan,
        album,
        tracks,
    })
}

pub(super) async fn plan(
    State(state): State<AppState>,
    Json(body): Json<IngestParams>,
) -> Result<impl IntoResponse, AppError> {
    let Built { plan, album, .. } = build(&state, &body).await?;

    Ok(Json(json!({
        "plan": plan,
        "album": album,
        "audio_count": plan.audio_count(),
        "conflicts": plan.conflicts(),
    })))
}

// -- Apply --

#[derive(Deserialize)]
pub(super) struct ApplyBody {
    #[serde(flatten)]
    params: IngestParams,
    conflict_policy: Option<String>,
    /// Write the (possibly corrected) fields into the placed files.
    write_tags: Option<bool>,
}

pub(super) async fn apply(
    State(state): State<AppState>,
    Json(body): Json<ApplyBody>,
) -> Result<impl IntoResponse, AppError> {
    let Built {
        plan,
        album,
        tracks,
    } = build(&state, &body.params).await?;

    let settings = load_settings(&state);
    let policy = match body.conflict_policy.as_deref() {
        Some("overwrite") => ConflictPolicy::Overwrite,
        Some("rename") => ConflictPolicy::Rename,
        Some("skip") => ConflictPolicy::Skip,
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "unknown conflict_policy '{other}'"
            )));
        }
        None => settings.conflict_policy,
    };
    let write_tags = body.write_tags.unwrap_or(settings.write_tags);

    let job_id = new_job_id();
    save_job(
        &state,
        &job_id,
        json!({
            "id": job_id,
            "status": "running",
            "source_path": plan.source_path,
            "dest_root": plan.dest_root,
            "album_dir": plan.album_dir,
            "mode": plan.mode,
            "started_at": now_iso(),
            "total": plan.entries.len(),
        }),
    );

    let state_bg = state.clone();
    let jid = job_id.clone();
    let album_bg = album.clone();
    tokio::spawn(async move {
        run_job(state_bg, jid, plan, policy, write_tags, album_bg, tracks).await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job_id, "status": "running" })),
    ))
}

/// Execute a plan, tag what landed, then let the scanner pick the album up.
async fn run_job(
    state: AppState,
    job_id: String,
    plan: IngestPlan,
    policy: ConflictPolicy,
    write_tags: bool,
    album: AlbumSummary,
    tracks: Vec<SourceTrack>,
) {
    let plan_for_exec = plan.clone();
    let report =
        match tokio::task::spawn_blocking(move || ingest::execute(&plan_for_exec, policy)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(job = %job_id, error = %e, "ingest_execute_panicked");
                save_job(
                    &state,
                    &job_id,
                    json!({
                        "id": job_id,
                        "status": "failed",
                        "error": format!("{e}"),
                        "finished_at": now_iso(),
                    }),
                );
                return;
            }
        };

    let tags_written = if write_tags {
        write_placed_tags(&report, &album, &tracks).await
    } else {
        0
    };

    // Targeted scan so the album appears without a full library rescan. Skipped
    // when nothing landed, to avoid kicking off a scan for a no-op job.
    let scanned = if report.files_placed() > 0 {
        if let Some(ref dir) = report.album_dir {
            crate::routes::system::scan::spawn_library_scan(state.clone(), false, Some(dir.clone()))
                .await
        } else {
            false
        }
    } else {
        false
    };

    tracing::info!(
        job = %job_id,
        placed = report.files_placed(),
        errors = report.errors.len(),
        skipped = report.skipped.len(),
        mode = report.mode.as_str(),
        "ingest_job_finished"
    );

    save_job(
        &state,
        &job_id,
        json!({
            "id": job_id,
            "status": if report.errors.is_empty() { "done" } else { "partial" },
            "source_path": plan.source_path,
            "dest_root": plan.dest_root,
            "album_dir": report.album_dir,
            "mode": report.mode,
            "finished_at": now_iso(),
            "placed": report.files_placed(),
            "bytes": report.bytes,
            "tags_written": tags_written,
            "scan_triggered": scanned,
            "report": report,
            "album": {
                "album": album.album,
                "album_artist": album.album_artist,
                "year": album.year,
            },
        }),
    );
}

/// Write the confirmed fields onto the files that landed.
///
/// Album-wide values go on every file; title, track and disc come from the
/// matching source track, which carries whatever the user accepted from a
/// chosen release listing. A file is only touched when there is something to
/// write, so an already-correct album is not rewritten for nothing.
///
/// The per-file lookup is keyed on the *source* path, since `tracks` describes
/// the files before the move — `report.moved` holds both ends of each pair.
async fn write_placed_tags(
    report: &IngestReport,
    album: &AlbumSummary,
    tracks: &[SourceTrack],
) -> usize {
    let by_source: HashMap<&str, &SourceTrack> =
        tracks.iter().map(|t| (t.source_path.as_str(), t)).collect();

    let mut jobs: Vec<(String, tune_core::metadata::MetadataUpdate)> = Vec::new();
    for moved in &report.moved {
        // Sidecars carry no tags.
        if ingest::is_extra_file(Path::new(&moved.dest_path)) {
            continue;
        }
        let source = by_source.get(moved.source_path.as_str());

        let update = tune_core::metadata::MetadataUpdate {
            album: album.album.clone(),
            album_artist: album.album_artist.clone(),
            artist: None,
            title: source.and_then(|s| s.title.clone()),
            genre: album.genre.clone(),
            track_number: source.and_then(|s| s.track_number),
            disc_number: source.and_then(|s| s.disc_number),
            year: album.year,
            composer: None,
            label: None,
        };

        let nothing_to_write = update.album.is_none()
            && update.album_artist.is_none()
            && update.year.is_none()
            && update.genre.is_none()
            && update.title.is_none()
            && update.track_number.is_none()
            && update.disc_number.is_none();
        if nothing_to_write {
            continue;
        }

        jobs.push((moved.dest_path.clone(), update));
    }

    if jobs.is_empty() {
        return 0;
    }

    tokio::task::spawn_blocking(move || {
        let mut written = 0usize;
        for (path, update) in jobs {
            match tune_core::metadata::write_metadata(Path::new(&path), &update) {
                Ok(()) => written += 1,
                Err(e) => tracing::warn!(path = %path, error = %e, "ingest_tag_write_failed"),
            }
        }
        written
    })
    .await
    .unwrap_or(0)
}

// -- Jobs & undo --

pub(super) async fn list_jobs(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let ids = job_index(&state);
    let jobs: Vec<Value> = ids.iter().filter_map(|id| load_job(&state, id)).collect();
    Ok(Json(json!({ "jobs": jobs })))
}

pub(super) async fn get_job(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    load_job(&state, &id)
        .map(Json)
        .ok_or_else(|| AppError::not_found(format!("no ingest job {id}")))
}

pub(super) async fn undo_job(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let job =
        load_job(&state, &id).ok_or_else(|| AppError::not_found(format!("no ingest job {id}")))?;

    let status = job.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "running" {
        return Err(AppError::conflict("job is still running"));
    }
    if status == "undone" {
        return Err(AppError::conflict("job was already undone"));
    }

    let report: IngestReport = job
        .get("report")
        .cloned()
        .and_then(|r| serde_json::from_value(r).ok())
        .ok_or_else(|| AppError::bad_request("job has no undo manifest"))?;

    let album_dir = report.album_dir.clone();
    let result = tokio::task::spawn_blocking(move || ingest::undo(&report))
        .await
        .map_err(|e| AppError::internal(format!("undo: {e}")))?;

    // The library still lists tracks at paths that no longer exist; a targeted
    // rescan of the (now empty) album folder prunes them.
    if let Some(dir) = album_dir {
        crate::routes::system::scan::spawn_library_scan(state.clone(), false, Some(dir)).await;
    }

    let mut updated = job.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("status".into(), json!("undone"));
        obj.insert("undone_at".into(), json!(now_iso()));
        obj.insert("undo_reverted".into(), json!(result.moved.len()));
        obj.insert("undo_errors".into(), json!(result.errors));
    }
    save_job(&state, &id, updated.clone());

    Ok(Json(json!({
        "job_id": id,
        "reverted": result.moved.len(),
        "errors": result.errors,
    })))
}

// -- Upload (drag & drop from a browser) --

/// Where uploaded files are staged before being ingested.
///
/// A browser drop hands over bytes, not paths, so the files must land
/// server-side first. Kept next to the artwork cache — a writable data
/// directory on every platform we ship.
fn staging_dir() -> PathBuf {
    if let Ok(v) = std::env::var("TUNE_INGEST_STAGING") {
        return PathBuf::from(v);
    }
    super::artwork_cache_dir().with_file_name("ingest_staging")
}

/// Accept dropped files and stage them in a fresh folder.
///
/// Each part's filename is sanitized component by component: a browser can
/// send `../../etc/passwd` as a relative path in a folder drop, and a client
/// is never trusted with a destination.
pub(super) async fn upload(mut multipart: Multipart) -> Result<impl IntoResponse, AppError> {
    let batch = staging_dir().join(new_job_id());
    std::fs::create_dir_all(&batch)
        .map_err(|e| AppError::internal(format!("create staging dir: {e}")))?;

    let mut written: Vec<String> = Vec::new();
    let mut bytes_total: u64 = 0;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("malformed upload: {e}")))?
    {
        let Some(raw_name) = field.file_name().map(String::from) else {
            continue;
        };

        let safe_rel: PathBuf = raw_name
            .replace('\\', "/")
            .split('/')
            .filter(|c| !c.is_empty() && *c != "." && *c != "..")
            .map(ingest::sanitize_component)
            .collect();
        if safe_rel.as_os_str().is_empty() {
            continue;
        }

        let dest = batch.join(&safe_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AppError::internal(format!("create dir: {e}")))?;
        }

        // Stream to disk chunk by chunk: a dropped album is far too big to
        // buffer whole in memory.
        let mut file = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| AppError::internal(format!("create {}: {e}", dest.display())))?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::bad_request(format!("upload interrupted: {e}")))?
        {
            bytes_total += chunk.len() as u64;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|e| AppError::internal(format!("write: {e}")))?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|e| AppError::internal(format!("flush: {e}")))?;

        written.push(dest.to_string_lossy().to_string());
    }

    if written.is_empty() {
        let _ = std::fs::remove_dir_all(&batch);
        return Err(AppError::bad_request("no files in the upload"));
    }

    tracing::info!(
        files = written.len(),
        bytes = bytes_total,
        dir = %batch.display(),
        "ingest_upload_staged"
    );

    Ok(Json(json!({
        // Feed this straight back into /analyze.
        "source_path": batch.to_string_lossy(),
        "files": written.len(),
        "bytes": bytes_total,
    })))
}

// -- Job persistence (settings table, like the other import tasks) --

fn job_key(id: &str) -> String {
    format!("ingest_job_{id}")
}

fn save_job(state: &AppState, id: &str, value: Value) {
    let repo = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.set(&job_key(id), &value.to_string()) {
        tracing::warn!(job = %id, error = %e, "ingest_job_save_failed");
        return;
    }

    let mut ids = job_index(state);
    ids.retain(|existing| existing != id);
    ids.insert(0, id.to_string());

    // Drop the oldest jobs, records and all, so settings does not grow forever.
    for stale in ids.iter().skip(MAX_JOB_HISTORY) {
        let _ = repo.delete(&job_key(stale));
    }
    ids.truncate(MAX_JOB_HISTORY);

    if let Ok(serialized) = serde_json::to_string(&ids) {
        let _ = repo.set(KEY_JOB_INDEX, &serialized);
    }
}

fn load_job(state: &AppState, id: &str) -> Option<Value> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(&job_key(id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn job_index(state: &AppState) -> Vec<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(KEY_JOB_INDEX)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn new_job_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn now_iso() -> String {
    super::now_iso_utc()
}
