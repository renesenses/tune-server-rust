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

    /// Le vocabulaire que l'écran web attend dans `state` (#3002).
    ///
    /// `api.ts` déclare `state: 'converting' | 'done' | 'error'` et
    /// `ConverterView.svelte` ne teste que `=== 'done'` et `=== 'error'`.
    /// `status` garde son vocabulaire d'origine : on **traduit**, on ne
    /// renomme pas — un consommateur qui lit déjà `status` ne doit rien voir
    /// bouger.
    ///
    /// `cancelled` ne figure pas dans l'union déclarée par le web : l'écran
    /// arrête son sondage avant, en local. Le rendre quand même est honnête et
    /// sans conséquence — mieux vaut un état vrai qu'un état plié à une union
    /// incomplète.
    fn web_state(&self) -> &'static str {
        match self {
            Self::Running => "converting",
            Self::Completed => "done",
            Self::Failed => "error",
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
    /// Octets écrits dans `output_dir`, cumulés au fil des conversions.
    ///
    /// Le web affiche « Télécharger (ZIP, 128,4 Mo) » : il lui faut une taille
    /// AVANT de demander l'archive. La construire à chaque sondage (toutes les
    /// 1,5 s) serait absurde ; on additionne donc ce qu'on vient d'écrire.
    /// C'est la taille des fichiers convertis, pas celle du ZIP — pour du FLAC,
    /// du MP3 ou de l'AAC déjà compressés, `zip` ne gagne quasiment rien, donc
    /// l'écart se compte en pour mille.
    output_bytes: u64,
}

type JobStore = Arc<Mutex<HashMap<String, Arc<Mutex<ConvertJob>>>>>;

/// Root of the per-job scratch directories the converter writes into.
///
/// Single source of truth: the directory `start_job` creates and the one
/// `/capabilities` announces must never drift apart (#2943).
const CONVERT_OUTPUT_ROOT: &str = "/tmp/tune-convert";

/// Where one job's converted files land. Nothing is ever written outside of
/// this directory — not into the library, not next to the source files.
fn job_output_dir(job_id: &str) -> PathBuf {
    PathBuf::from(CONVERT_OUTPUT_ROOT).join(job_id)
}

/// What the converter does with the result, and what it promises not to touch.
///
/// #2943 (Bilou, fil forum 1095): « On ne sait d'ailleurs pas comment se
/// réalise la conversion proprement dit : duplication de l'album ?,
/// remplacement ? ». He had to ask on the forum, and wait five weeks, because
/// the server never stated it anywhere — the screen had nothing truthful to
/// display before launching a job that touches music files.
///
/// Everything below is knowable here and only here:
/// - `run_conversion` writes exclusively into `job_output_dir(job_id)`;
/// - the only way out is the ZIP built by `download_job`/`build_zip`;
/// - sources are opened read-only (decode, plus `copy_tags`, which reads the
///   source tags and saves them onto the *destination* file).
///
/// Declaring it lets the screen quote the server instead of hardcoding a claim
/// that a future destination setting (#2944) would silently turn into a lie.
fn delivery_descriptor() -> Value {
    json!({
        // The result leaves the server as a single downloadable archive.
        "mode": "zip_download",
        // Server-side scratch root; per job, a `{output_root}/{job_id}` dir.
        "output_root": CONVERT_OUTPUT_ROOT,
        // The two questions the screen must be able to answer up front.
        "writes_to_library": false,
        "modifies_sources": false,
    })
}

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
    Json(capabilities_payload().await)
}

/// The payload behind `GET /capabilities`, split out so the contract can be
/// asserted without going through an HTTP response body.
async fn capabilities_payload() -> Value {
    let ffmpeg = resolve_tool("ffmpeg");
    let lame = resolve_tool("lame");
    let encoders = match &ffmpeg {
        Some(path) => ffmpeg_encoders(path).await,
        None => std::collections::HashSet::new(),
    };

    json!({
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
        // Where the result goes and what stays untouched (#2943).
        "delivery": delivery_descriptor(),
    })
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
    let output_dir = job_output_dir(&job_id);
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
        output_bytes: 0,
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

/// Taille lisible, telle que le web la recopie : « Télécharger (ZIP, 128.4 MB) ».
///
/// Unités décimales (1 kB = 1000 o), celles qu'affichent les explorateurs de
/// fichiers et les convertisseurs audio. La chaîne est fabriquée ici parce que
/// le web déclare `download_size?: string` : elle n'est donc PAS traduisible.
/// D'où des unités internationales plutôt que « Mo », qui jureraient dans
/// l'interface anglaise.
fn taille_lisible(octets: u64) -> String {
    const UNITES: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut valeur = octets as f64;
    let mut rang = 0;
    while valeur >= 1000.0 && rang < UNITES.len() - 1 {
        valeur /= 1000.0;
        rang += 1;
    }
    if rang == 0 {
        format!("{octets} B")
    } else {
        format!("{valeur:.1} {}", UNITES[rang])
    }
}

/// Le corps de `GET /converter/status/{id}`, dans la forme que l'écran
/// Convertisseur lit réellement (#3002).
///
/// Le serveur ne rendait que `status`, `total`, `completed`, `current_file` et
/// `errors`. `ConverterView.svelte` lit `state`, `progress`, `converted`,
/// `download_size` et `error` : **deux champs sur six se rencontraient**.
/// Chaque lecture portant un `??`, rien n'échouait — et c'est précisément ce
/// qui rendait le défaut invisible : la barre restait à 0 %, le compteur à
/// 0/N, `state === 'done'` n'était jamais vrai, donc le bouton de
/// téléchargement ne s'affichait **jamais** alors que l'archive était prête.
/// Une conversion réussie était indiscernable d'une conversion bloquée.
///
/// Les cinq champs historiques sont **conservés**. On ajoute, on ne renomme
/// pas : `converted` double `completed` et `progress` se déduit de
/// `completed/total`, mais retirer l'ancienne forme casserait tout
/// consommateur qui la lit déjà. Le doublon est le prix de la compatibilité.
///
/// Fonction pure et séparée du gestionnaire pour être éprouvable sans routeur,
/// sans licence premium et sans fichier audio — voir les tests en fin de
/// fichier, qui confrontent ce corps à `docs/contrat-web.json`.
fn payload_statut(job_id: &str, job: &ConvertJob) -> Value {
    let errors: Vec<Value> = job
        .errors
        .iter()
        .map(|e| json!({"file": e.file, "message": e.message}))
        .collect();

    // `start_job` refuse une liste de sources vide (400), donc `total >= 1` en
    // pratique ; la garde évite quand même une division par zéro.
    let progress = if job.total == 0 {
        0.0
    } else {
        (job.completed as f64 / job.total as f64 * 100.0).clamp(0.0, 100.0)
    };

    // Le web fait `conversionError = status.error ?? <message par défaut>` :
    // `null` tant que rien n'a échoué lui laisse donc son propre libellé,
    // traduit. On ne parle que lorsqu'on a quelque chose à dire.
    let error = match (&job.status, job.errors.first()) {
        (JobStatus::Failed, Some(premier)) => {
            Some(format!("{}: {}", premier.file, premier.message))
        }
        (JobStatus::Failed, None) => Some("conversion failed".to_string()),
        _ => None,
    };

    json!({
        "job_id": job_id,
        // Forme historique — conservée telle quelle.
        "status": job.status.as_str(),
        "total": job.total,
        "completed": job.completed,
        "current_file": job.current_file,
        "errors": errors,
        // Forme lue par l'écran web (#3002).
        "state": job.status.web_state(),
        "progress": progress,
        "converted": job.completed,
        "download_size": taille_lisible(job.output_bytes),
        "error": error,
    })
}

async fn job_status(AxumPath(job_id): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let store = job_store();
    let map = store.lock().await;
    let job_arc = map
        .get(&job_id)
        .ok_or_else(|| AppError::not_found(format!("job not found: {job_id}")))?
        .clone();
    let job = job_arc.lock().await;

    Ok(Json(payload_statut(&job_id, &job)))
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

                // Taille du fichier qu'on vient d'écrire : elle alimente le
                // « (ZIP, …) » du bouton de téléchargement (#3002). Un
                // `metadata` illisible ne doit rien casser — on compte 0 et on
                // continue, la conversion, elle, a réussi.
                let ecrits = tokio::fs::metadata(&out_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);

                let mut j = job.lock().await;
                j.completed += 1;
                j.output_bytes += ecrits;
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
    fn la_destination_et_ce_qui_nest_pas_touche_sont_annonces() {
        // #2943 : la question de Bilou — « duplication de l'album ?,
        // remplacement ? » — doit trouver sa réponse dans le contrat serveur.
        let d = delivery_descriptor();
        assert_eq!(
            d["mode"], "zip_download",
            "le résultat sort en archive téléchargeable, l'écran doit pouvoir le dire"
        );
        assert_eq!(d["output_root"], CONVERT_OUTPUT_ROOT);
        assert_eq!(
            d["writes_to_library"], false,
            "la bibliothèque n'est ni modifiée ni dupliquée"
        );
        assert_eq!(
            d["modifies_sources"], false,
            "les fichiers d'origine ne sont ouverts qu'en lecture"
        );
    }

    #[test]
    fn le_dossier_dun_travail_reste_sous_la_racine_annoncee() {
        // Témoin anti-dérive : si quelqu'un déplace la sortie sans corriger
        // l'annonce, l'écran mentirait. Ce test devient rouge d'abord.
        let announced = delivery_descriptor()["output_root"]
            .as_str()
            .expect("output_root est une chaîne")
            .to_string();
        let dir = job_output_dir("11111111-2222-3333-4444-555555555555");
        assert!(
            dir.starts_with(&announced),
            "sortie {} hors de la racine annoncée {announced}",
            dir.display()
        );
        assert_ne!(
            dir,
            PathBuf::from(&announced),
            "chaque travail a son propre sous-dossier"
        );
    }

    #[tokio::test]
    async fn capabilities_conserve_formats_et_outils_en_plus_de_la_destination() {
        // Témoin anti-régression : l'ajout de `delivery` ne doit rien retirer
        // du contrat de #1524 (grisage des formats indisponibles).
        let payload = capabilities_payload().await;
        assert!(payload["formats"]["flac"].is_boolean());
        assert!(payload["formats"]["mp3"].is_boolean());
        assert!(payload["tools"].is_object(), "bloc tools conservé");
        assert!(
            payload["tools"]
                .as_object()
                .is_some_and(|t| t.contains_key("ffmpeg")),
            "diagnostic ffmpeg conservé"
        );
        assert_eq!(payload["delivery"], delivery_descriptor());
    }

    /// La carte web commitée, la même que lit `tests/web_response_contracts.rs`.
    ///
    /// `GET /converter/status/{id}` ne peut PAS être joué par ce banc-là : la
    /// route amont exige une licence premium et de vrais fichiers audio. On
    /// confronte donc le corps à la carte ici, où `payload_statut` est
    /// atteignable sans routeur — même source de vérité, autre porte d'entrée.
    const CARTE_WEB: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/contrat-web.json"
    ));

    fn champs_exiges_par_le_web() -> Vec<String> {
        let carte: Value = serde_json::from_str(CARTE_WEB).expect("carte contrat web lisible");
        let contrat = carte["routes"]
            .as_array()
            .expect("la carte porte un tableau `routes`")
            .iter()
            .find(|c| c["route"] == "/converter/status/{}" && c["methode"] == "GET")
            .expect(
                "GET /converter/status/{} doit figurer dans docs/contrat-web.json — \
                 sans entrée, aucun contrôle ne peut voir la dérive (#3002)",
            );
        contrat["champs_obligatoires"]
            .as_array()
            .expect("champs_obligatoires est un tableau")
            .iter()
            .map(|c| c.as_str().expect("nom de champ textuel").to_string())
            .collect()
    }

    fn job_temoin(status: JobStatus, completed: usize) -> ConvertJob {
        ConvertJob {
            status,
            total: 4,
            completed,
            current_file: "02 - Chelsea Girl.flac".into(),
            errors: Vec::new(),
            output_dir: PathBuf::from("/tmp/tune-convert/temoin"),
            output_bytes: 128_400_000,
        }
    }

    #[test]
    fn le_statut_porte_tous_les_champs_que_lecran_convertisseur_lit() {
        // #3002 : le serveur rendait `status`, `total`, `completed`,
        // `current_file`, `errors` ; `ConverterView.svelte` lit `state`,
        // `progress`, `converted`, `download_size` et `error`. Deux champs sur
        // six se rencontraient, chaque lecture portait un `??`, donc RIEN ne
        // cassait — la barre restait à 0 %, le compteur à 0/N, et le bouton de
        // téléchargement ne s'affichait jamais.
        let exiges = champs_exiges_par_le_web();
        assert!(
            exiges.len() >= 11,
            "le contrat s'est appauvri : {exiges:?} — il doit couvrir la forme \
             historique ET celle que lit le web"
        );

        for etat in [
            JobStatus::Running,
            JobStatus::Completed,
            JobStatus::Failed,
            JobStatus::Cancelled,
        ] {
            let job = job_temoin(etat.clone(), 2);
            let payload = payload_statut("job-temoin", &job);
            let objet = payload.as_object().expect("corps JSON objet");
            for champ in &exiges {
                assert!(
                    objet.contains_key(champ),
                    "état {} : champ obligatoire absent du corps : {champ} — \
                     corps={payload}",
                    etat.as_str()
                );
            }
        }
    }

    #[test]
    fn une_conversion_terminee_ne_ressemble_plus_a_une_conversion_bloquee() {
        // Le cœur du défaut : avant, `state`, `progress` et `converted`
        // valaient toujours undefined/0/0, donc une réussite et un blocage
        // rendaient exactement le même écran.
        let en_cours = payload_statut("j", &job_temoin(JobStatus::Running, 1));
        assert_eq!(en_cours["state"], "converting");
        assert_eq!(en_cours["progress"], 25.0);
        assert_eq!(en_cours["converted"], 1);
        assert!(
            en_cours["error"].is_null(),
            "rien n'a échoué : le web doit garder son propre libellé traduit"
        );

        let fini = payload_statut("j", &job_temoin(JobStatus::Completed, 4));
        assert_eq!(
            fini["state"], "done",
            "sans `done`, le bouton de téléchargement ne s'affiche jamais"
        );
        assert_eq!(fini["progress"], 100.0);
        assert_eq!(fini["converted"], 4);
        assert_eq!(fini["download_size"], "128.4 MB");

        // Les cinq champs historiques ne bougent pas : on ajoute, on ne
        // renomme pas. Un consommateur qui lit `status`/`completed` doit
        // continuer de fonctionner à l'identique.
        assert_eq!(fini["status"], "completed");
        assert_eq!(fini["completed"], 4);
        assert!(fini["errors"].is_array());
    }

    #[test]
    fn un_echec_est_annonce_comme_tel_et_porte_son_motif() {
        let mut job = job_temoin(JobStatus::Failed, 4);
        job.errors.push(JobError {
            file: "/music/x.wma".into(),
            message: "unsupported input".into(),
        });
        let payload = payload_statut("j", &job);
        assert_eq!(
            payload["state"], "error",
            "`state === 'error'` est la seule voie par laquelle l'écran sort \
             du sondage : sans lui, un échec tourne indéfiniment"
        );
        assert_eq!(payload["error"], "/music/x.wma: unsupported input");
    }

    #[test]
    fn la_taille_lisible_suit_les_unites_que_le_web_recopie() {
        assert_eq!(taille_lisible(0), "0 B");
        assert_eq!(taille_lisible(999), "999 B");
        assert_eq!(taille_lisible(1_000), "1.0 kB");
        assert_eq!(taille_lisible(128_400_000), "128.4 MB");
        assert_eq!(taille_lisible(2_500_000_000), "2.5 GB");
    }

    #[test]
    fn un_travail_sans_source_ne_divise_pas_par_zero() {
        // `start_job` refuse une liste vide (400), mais le corps ne doit pas
        // dépendre de cette garde-là pour rester calculable.
        let mut job = job_temoin(JobStatus::Completed, 0);
        job.total = 0;
        assert_eq!(payload_statut("j", &job)["progress"], 0.0);
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
