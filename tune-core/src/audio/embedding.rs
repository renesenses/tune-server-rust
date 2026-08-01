//! CLAP audio embeddings for the acoustic "sounds-like" Smart Radio.
//!
//! The CLAP audio tower is exported to ONNX with its mel front-end baked into
//! the graph, so this side just feeds a raw 48 kHz mono waveform and gets a
//! 512-d embedding back — no spectrogram to reimplement, bit-exact with the
//! reference (validated in the T0 spike). onnxruntime is loaded dynamically at
//! runtime (`load-dynamic`), so the build never pulls a target-specific binary.
//!
//! Feature-gated behind `audio-embedding`: the default build does not link ort.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ort::session::Session;
use ort::value::Tensor;
use tracing::{info, warn};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::track_metadata_repo::TrackMetadataRepo;

/// Tracks embedded per batch before yielding, mirroring the ReplayGain sweep.
const TRACK_BATCH: usize = 32;
/// Pause between files so the background pass never saturates the CPU.
const PER_FILE_PAUSE_MS: u64 = 50;
/// Sentinel key stamped on every processed track (success OR failure) so an
/// undecodable file is not retried forever — same idiom as `rg_analyzed`.
const SENTINEL: &str = "audio_embed_analyzed";

/// Samples fed to the model: 10 s @ 48 kHz mono, matching CLAP's fixed window.
const WINDOW_SAMPLES: usize = 480_000;

// Storage layout, constants and cosine live in the always-compiled read side.
use super::embedding_store::{self, EMBED_DIM, MODEL_ID};

/// A loaded CLAP audio embedder. Cheap to reuse across many tracks in the
/// background analysis pass; `embed` is the per-track hot path.
pub struct AudioEmbedder {
    session: Session,
}

impl AudioEmbedder {
    /// Load the ONNX model from disk. The onnxruntime shared library must be
    /// resolvable at runtime (bundled/downloaded next to the model on the
    /// opt-in activation path).
    pub fn load(model_path: &Path) -> Result<Self, String> {
        let session = Session::builder()
            .map_err(|e| format!("ort builder: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| format!("ort load {}: {e}", model_path.display()))?;
        Ok(Self { session })
    }

    /// Embed a mono 48 kHz waveform into a normalised 512-d vector.
    ///
    /// The waveform is repeat-padded / truncated to the fixed 10 s window, the
    /// same `repeatpad` rule CLAP applies, so a short track is tiled rather than
    /// zero-padded (which would dilute its timbre with silence).
    pub fn embed(&mut self, waveform: &[f32]) -> Result<Vec<f32>, String> {
        if waveform.is_empty() {
            return Err("empty waveform".into());
        }
        let mut buf = vec![0f32; WINDOW_SAMPLES];
        for (i, s) in buf.iter_mut().enumerate() {
            *s = waveform[i % waveform.len()];
        }

        let input = Tensor::from_array(([1usize, WINDOW_SAMPLES], buf))
            .map_err(|e| format!("ort input tensor: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["waveform" => input])
            .map_err(|e| format!("ort run: {e}"))?;
        let (_shape, data) = outputs["embedding"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("ort extract: {e}"))?;

        let mut v: Vec<f32> = data.to_vec();
        if v.len() != EMBED_DIM {
            return Err(format!(
                "unexpected embedding dim {} (want {EMBED_DIM})",
                v.len()
            ));
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in &mut v {
            *x /= norm;
        }
        Ok(v)
    }
}

/// Embed up to `TRACK_BATCH` local tracks that have no audio embedding yet.
/// Returns how many were processed (0 ⇒ nothing left, caller idles). Mirrors
/// `replaygain::analyze_track_batch`: bounded, throttled, resumable via the
/// `audio_embed_analyzed` sentinel.
pub async fn analyze_embedding_batch(
    backend: &Arc<dyn DbBackend>,
    embedder: &mut AudioEmbedder,
) -> usize {
    let rows = match backend.query_many(
        "SELECT t.id, t.file_path FROM tracks t \
         WHERE t.file_path IS NOT NULL AND t.file_path != '' \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'audio_embed_analyzed') \
         LIMIT ?",
        &[&(TRACK_BATCH as i64) as &dyn ToSqlValue],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "audio_embed_candidate_query_failed");
            return 0;
        }
    };
    if rows.is_empty() {
        return 0;
    }

    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;

    let mut done = 0usize;
    for r in &rows {
        let track_id = match r.first().and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => continue,
        };
        let path = match r.get(1).and_then(|v| v.as_string()) {
            Some(p) if !p.is_empty() => p,
            _ => continue,
        };

        // Decode the first 10 s at 48 kHz mono (CLAP's window) off the async
        // runtime; the returned samples carry the source bit depth for scaling.
        let p = path.clone();
        let decoded = tokio::task::spawn_blocking(move || {
            crate::audio::decode::decode_to_pcm(&p, Some(48_000), Some(1), 0.0, 10.0)
        })
        .await;

        if let Ok(Ok(d)) = decoded {
            // i32 samples → f32 in [-1, 1], the same full-scale rule the loudness
            // pass uses (sample / 2^(bits-1)); librosa normalises identically.
            let scale = (1i64 << (d.bit_depth.saturating_sub(1)).min(31)) as f32;
            let wav: Vec<f32> = d.samples_i32.iter().map(|&s| s as f32 / scale).collect();
            match embedder.embed(&wav) {
                Ok(emb) => {
                    let row = vec![
                        SqlValue::Int(track_id),
                        SqlValue::Text(MODEL_ID.to_string()),
                        SqlValue::Blob(embedding_store::to_bytes(&emb)),
                        SqlValue::Int(now),
                    ];
                    // Portable upsert (SQLite ≥ 3.24 + PG): track_id is the PK.
                    let _ = backend
                        .execute_many(
                            "INSERT INTO track_audio_embedding \
                             (track_id, model, embedding, analyzed_at) VALUES (?, ?, ?, ?) \
                             ON CONFLICT (track_id) DO UPDATE SET \
                             model = excluded.model, embedding = excluded.embedding, \
                             analyzed_at = excluded.analyzed_at",
                            &[row],
                        )
                        .into_iter()
                        .next();
                }
                Err(e) => warn!(track_id, error = %e, "audio_embed_infer_failed"),
            }
        }

        // Stamp the sentinel whether or not it produced a vector, so a broken
        // or silent file drops out of the sweep instead of being retried.
        let _ = repo.set(track_id, SENTINEL, &now.to_string());
        done += 1;
        tokio::time::sleep(std::time::Duration::from_millis(PER_FILE_PAUSE_MS)).await;
    }

    info!(embedded = done, "audio_embedding_batch");
    done
}

/// Idle wait when disabled or the sweep is drained.
const IDLE_SLEEP_SECS: u64 = 900;
/// Opt-in gate: only "true" enables it (heavy, needs the model downloaded).
const ENABLED_KEY: &str = "audio_embedding_enabled";
/// Where the model file goes; falls back to `TUNE_AUDIO_EMBED_MODEL`.
const MODEL_PATH_KEY: &str = "audio_embedding_model_path";
/// Published CLAP audio tower (ONNX, mel in-graph, 512-d). Fetched on first
/// activation and cached at the configured path.
const MODEL_URL: &str = "https://github.com/renesenses/tune-server-rust/releases/download/models/clap-audio-2023/clap-audio-2023.onnx";
/// SHA-256 of the model above — a corrupt/partial download is rejected.
const MODEL_SHA256: &str = "31c37def7ff2a9a36da07667bc7412dfb5ca16e081673055e45d3ac26de387d9";

fn enabled(settings: &crate::db::settings_repo::SettingsRepo) -> bool {
    settings.get(ENABLED_KEY).ok().flatten().as_deref() == Some("true")
}

/// The configured model destination (setting → env). Not required to exist —
/// [`ensure_model`] downloads it there on first run.
fn configured_model_path(settings: &crate::db::settings_repo::SettingsRepo) -> Option<PathBuf> {
    settings
        .get(MODEL_PATH_KEY)
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_AUDIO_EMBED_MODEL").ok())
        .map(PathBuf::from)
}

/// Make sure the model exists at `dest`, downloading + checksum-verifying it if
/// absent. Idempotent: an already-present file is trusted (verified on the run
/// that wrote it). Written atomically via a temp file so a killed download
/// never leaves a truncated model in place.
async fn ensure_model(dest: &Path) -> Result<(), String> {
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    info!(url = MODEL_URL, dest = %dest.display(), "audio_model_downloading");
    let bytes = reqwest::get(MODEL_URL)
        .await
        .map_err(|e| format!("download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download status: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download body: {e}"))?;

    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::new_with_prefix(&bytes).finalize());
    if got != MODEL_SHA256 {
        return Err(format!("checksum mismatch: got {got}, want {MODEL_SHA256}"));
    }

    let tmp = dest.with_extension("onnx.part");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("rename: {e}"))?;
    info!(dest = %dest.display(), bytes = bytes.len(), "audio_model_ready");
    Ok(())
}

/// Provision the onnxruntime shared lib next to the model and load it into `ort`
/// exactly once (`ort::init_from` must run before the first `Session`). The
/// dylib is cached under the model's directory, so it is fetched only on the
/// first activation. Once `*ready` is set the runtime is live for the process.
async fn ensure_runtime_loaded(model_path: &Path, ready: &mut bool) -> Result<(), String> {
    if *ready {
        return Ok(());
    }
    let cache_root = model_path.parent().unwrap_or_else(|| Path::new("."));
    let dylib = super::runtime::ensure_runtime(cache_root).await?;
    ort::init_from(&dylib)
        .map_err(|e| format!("ort init_from {}: {e}", dylib.display()))?
        .commit();
    info!(dylib = %dylib.display(), "audio_runtime_loaded");
    *ready = true;
    Ok(())
}

/// Spawn the background audio-embedding sweep. Mirrors `replaygain::spawn`:
/// opt-in via `audio_embedding_enabled`, downloads + checksum-verifies the CLAP
/// model on first activation (to the configured path), then chips away at the
/// library in bounded batches. No-ops cheaply while disabled or model-less.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    use crate::db::settings_repo::SettingsRepo;
    tokio::spawn(async move {
        // Let startup/scan settle before touching the disk hard.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut embedder: Option<AudioEmbedder> = None;
        // The onnxruntime dylib is loaded once, globally, before any Session.
        let mut runtime_ready = false;
        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            if enabled(&settings) {
                if embedder.is_none() {
                    if let Some(p) = configured_model_path(&settings) {
                        if let Err(e) = ensure_model(&p).await {
                            warn!(error = %e, "audio_model_unavailable");
                        } else if let Err(e) = ensure_runtime_loaded(&p, &mut runtime_ready).await {
                            warn!(error = %e, "audio_runtime_unavailable");
                        } else {
                            match AudioEmbedder::load(&p) {
                                Ok(e) => {
                                    info!(model = %p.display(), "audio_embedder_loaded");
                                    embedder = Some(e);
                                }
                                Err(e) => warn!(error = %e, "audio_embedder_load_failed"),
                            }
                        }
                    }
                }
                if let Some(emb) = embedder.as_mut() {
                    let did = analyze_embedding_batch(&backend, emb).await;
                    if did > 0 {
                        // More to do — loop promptly; the per-file pauses throttle.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        continue;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(IDLE_SLEEP_SECS)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end provisioning proof: fetch the real pyke onnxruntime archive for
    /// THIS platform, LZMA2-decompress + untar it, load it into `ort`, download
    /// the published CLAP model, and embed a buffer — asserting a normalised 512-d
    /// vector comes out. Ignored by default (network + ~120 MB model + ~40 MB
    /// runtime). Run explicitly to validate the activation path on a new platform:
    ///   cargo test -p tune-core --features audio-embedding \
    ///     provision_and_embed -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn provision_and_embed() {
        let dir = std::env::temp_dir().join("tune-audio-embed-e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("clap-audio-2023.onnx");

        ensure_model(&model).await.expect("download model");
        let mut ready = false;
        ensure_runtime_loaded(&model, &mut ready)
            .await
            .expect("provision onnxruntime");
        assert!(ready);

        let mut embedder = AudioEmbedder::load(&model).expect("load embedder");
        // 1 s of quiet noise → a valid, finite, unit-norm embedding.
        let wav: Vec<f32> = (0..48_000)
            .map(|i| ((i % 97) as f32 / 97.0 - 0.5) * 0.01)
            .collect();
        let emb = embedder.embed(&wav).expect("embed");
        assert_eq!(emb.len(), EMBED_DIM);
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "embedding not unit-norm: {norm}");
        assert!(emb.iter().all(|x| x.is_finite()));
    }
}
