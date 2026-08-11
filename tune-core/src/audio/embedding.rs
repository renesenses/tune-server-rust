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

/// Réglage de débit de la passe acoustique : combien de machine elle a le droit
/// de prendre. L'analyse décode dix secondes par piste et fait tourner un
/// réseau de neurones dessus ; sur un Raspberry Pi ou un serveur qui sert aussi
/// la musique, la cadence par défaut se remarque à l'oreille comme au
/// ventilateur.
///
/// Le levier est la pause entre deux fichiers, pas un nombre de fils : la passe
/// est déjà séquentielle, et allonger la pause laisse la main au reste du
/// serveur sans jamais rien interrompre en cours de route.
const THROTTLE_KEY: &str = "audio_embedding_throttle";

/// Pause entre deux fichiers selon le réglage. `eco` divise la charge par huit
/// environ, `rapide` enchaîne sans pause — à réserver à une machine dédiée ou à
/// une nuit d'analyse.
fn per_file_pause_ms(settings: &crate::db::settings_repo::SettingsRepo) -> u64 {
    match settings
        .get(THROTTLE_KEY)
        .ok()
        .flatten()
        .as_deref()
        .unwrap_or("equilibre")
    {
        "eco" => 400,
        "rapide" => 0,
        // Toute valeur inconnue retombe sur l'équilibre : un réglage mal écrit
        // ne doit pas mettre la machine à genoux.
        _ => PER_FILE_PAUSE_MS,
    }
}
/// Sentinel key stamped on every processed track (success OR failure) so an
/// undecodable file is not retried forever — same idiom as `rg_analyzed`. Its
/// *value* is the `MODEL_ID` it was analysed under, so a model bump invalidates
/// it and the track is re-swept (see the candidate query below).
const SENTINEL: &str = "audio_embed_analyzed";

/// Samples fed to the model: 10 s @ 48 kHz mono, matching CLAP's fixed window.
const WINDOW_SAMPLES: usize = 480_000;

/// Hard cap on decoding one track. A 10 s window decodes in well under a second
/// even for hi-res PCM; anything past this is a stuck decoder, not slowness.
const DECODE_TIMEOUT_SECS: u64 = 30;

/// Memory the sweep needs available before it will start a batch.
///
/// Measured on .18 (2026-08-10, 16 GB, ~25 k tracks): with the sweep running the
/// process sat between 596 MB and 1224 MB. It oscillated inside that band for
/// two hours and came back down repeatedly — so this is a **footprint, not a
/// leak** (#1462, where a 1.1 GB "leak" was chased that never existed). Most of
/// it is the 287 MB model plus onnxruntime's activation arena for HTSAT-base
/// over a 480 000-sample window.
///
/// 16 GB absorbs that without noticing. A 2 GB Pi or a memory-capped container
/// does not — and there the OOM killer decides, silently, because
/// `Restart=always` brings the server straight back with nothing said. So we
/// decide instead: under this much available memory the sweep pauses and says
/// why, then resumes when there is room. Set generously above the observed
/// ceiling: being an hour late to embed costs nothing, being killed mid-write
/// costs a database.
const MIN_AVAILABLE_MB: u64 = 1536;

/// Memory available to *this* process, in MB.
///
/// Order matters. Inside a container `/proc/meminfo` reports the **host's**
/// memory, so a 1 GB-capped container would read 32 GB free and get OOM-killed
/// anyway. The cgroup limit is the honest number when there is one; only when
/// it is absent or unlimited (`max`) do we fall back to the host view.
///
/// `None` means "cannot tell" — on macOS and Windows there is no cheap
/// equivalent of `MemAvailable`, and a guard that cannot measure must not
/// pretend to. Those are also not the constrained targets; the Pis, the NAS
/// boxes and the containers are all Linux.
#[cfg(target_os = "linux")]
fn available_memory_mb() -> Option<u64> {
    fn read(path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    // cgroup v2, then v1, then the host.
    cgroup_available_mb(
        read("/sys/fs/cgroup/memory.max").as_deref(),
        read("/sys/fs/cgroup/memory.current").as_deref(),
    )
    .or_else(|| {
        cgroup_available_mb(
            read("/sys/fs/cgroup/memory/memory.limit_in_bytes").as_deref(),
            read("/sys/fs/cgroup/memory/memory.usage_in_bytes").as_deref(),
        )
    })
    .or_else(|| mem_available_mb(read("/proc/meminfo").as_deref()?))
}

/// Headroom left inside a cgroup memory cap, in MB, from the raw contents of
/// its limit and usage files.
///
/// `None` when there is no usable cap — the file is missing, unparseable, holds
/// cgroup v2's literal `max`, or holds v1's "unlimited" sentinel (`i64::MAX`
/// rounded down to a page, so absurdly large rather than a round number). In
/// all of those cases the caller must fall through to the host's own view
/// rather than invent a limit.
/// Only *called* on Linux, but deliberately compiled everywhere so its tests
/// run on every platform — the parsing is where the bugs live, and gating it
/// out of the macOS/Windows builds would mean it is only ever exercised on
/// the one platform nobody develops on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_available_mb(limit: Option<&str>, usage: Option<&str>) -> Option<u64> {
    let limit: u64 = limit?.trim().parse().ok()?;
    let usage: u64 = usage?.trim().parse().ok()?;
    if limit >= (1 << 62) {
        return None;
    }
    Some(limit.saturating_sub(usage) / (1024 * 1024))
}

/// `MemAvailable` from `/proc/meminfo`, in MB.
///
/// `MemAvailable`, not `MemFree`: the kernel's own estimate of what a new
/// allocation can actually get, page cache included. `MemFree` on a server that
/// has been up for a week reads near zero and would pause the sweep forever.
/// Only *called* on Linux, but deliberately compiled everywhere so its tests
/// run on every platform — the parsing is where the bugs live, and gating it
/// out of the macOS/Windows builds would mean it is only ever exercised on
/// the one platform nobody develops on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mem_available_mb(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb / 1024)
}

#[cfg(not(target_os = "linux"))]
fn available_memory_mb() -> Option<u64> {
    None
}

/// Resident size of this process in MB, for the batch log. Diagnostic only —
/// never a decision input. On 2026-08-10 the absence of this number is what let
/// a correlation ("memory rises while the pass runs") stand in for a cause.
#[cfg(target_os = "linux")]
fn process_rss_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4 / 1024)
}

#[cfg(not(target_os = "linux"))]
fn process_rss_mb() -> Option<u64> {
    None
}

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
    /// zero-padded (which would dilute its timbre with silence). Each sample is
    /// 16-bit quantised first — CLAP's reference loader round-trips audio through
    /// `int16` (`int16_to_float32(float32_to_int16(y))`), so a hi-res source must
    /// be dithered down to 16-bit to land on the same manifold the model was
    /// trained on. Skipping this quietly degrades cross-modal (text↔audio)
    /// alignment even though same-album structure survives.
    pub fn embed(&mut self, waveform: &[f32]) -> Result<Vec<f32>, String> {
        if waveform.is_empty() {
            return Err("empty waveform".into());
        }
        let mut buf = vec![0f32; WINDOW_SAMPLES];
        for (i, s) in buf.iter_mut().enumerate() {
            *s = quantize_i16(waveform[i % waveform.len()]);
        }

        let input = Tensor::from_array(([1usize, WINDOW_SAMPLES], buf))
            .map_err(|e| format!("ort input tensor: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["waveform" => input])
            .map_err(|e| format!("ort run: {e}"))?;
        let (_shape, data) = outputs["audio_embedding"]
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

/// 16-bit quantise a sample, reproducing CLAP's `int16_to_float32(float32_to_int16(y))`
/// round-trip exactly. `float32_to_int16` clips to [-1, 1] then scales by 32767
/// and truncates toward zero (numpy `.astype(np.int16)`); `int16_to_float32`
/// divides by 32767. Rust's `as i16` cast truncates toward zero identically, and
/// the clamp keeps it in range so the cast never saturates unexpectedly.
#[inline]
fn quantize_i16(x: f32) -> f32 {
    let q = (x.clamp(-1.0, 1.0) * 32767.0) as i16;
    q as f32 / 32767.0
}

/// Convert decoded interleaved i32 PCM into a **mono** f32 waveform in [-1, 1]
/// for the embedder. The full-scale rule is `sample / 2^(bits-1)` (matching the
/// loudness pass; librosa normalises identically).
///
/// CLAP expects mono. `decode_to_pcm` is asked for 1 channel, but some decoder
/// paths ignore that and return **interleaved stereo** (#1108). Fed as if it were
/// mono, L/R/L/R doubles the effective sample rate and smears the timbre — enough
/// to preserve same-album structure (Phase 1) yet wreck cross-modal alignment
/// (text↔audio, Phase 3). Averaging the channels per frame restores true mono.
fn to_mono_f32(samples: &[i32], channels: u32, bit_depth: u16) -> Vec<f32> {
    let scale = (1i64 << (bit_depth.saturating_sub(1)).min(31)) as f32;
    let ch = channels.max(1) as usize;
    if ch <= 1 {
        return samples.iter().map(|&s| s as f32 / scale).collect();
    }
    samples
        .chunks(ch)
        .map(|frame| {
            let sum: i64 = frame.iter().map(|&s| s as i64).sum();
            (sum as f32 / frame.len() as f32) / scale
        })
        .collect()
}

/// Embed up to `TRACK_BATCH` local tracks that have no audio embedding yet.
/// Returns how many were processed (0 ⇒ nothing left, caller idles). Mirrors
/// `replaygain::analyze_track_batch`: bounded, throttled, resumable via the
/// `audio_embed_analyzed` sentinel.
pub async fn analyze_embedding_batch(
    backend: &Arc<dyn DbBackend>,
    embedder: &mut AudioEmbedder,
) -> usize {
    // Skip DSD/DSF/DFF: the DSD→PCM resampler can spin on some SACD rips,
    // hanging the (non-cancellable) decode thread forever and freezing the whole
    // sweep. DSD is a small slice of a library and the decoder is fragile, so we
    // leave those without an acoustic embedding (smart_radio falls back to
    // metadata for them) rather than risk the stall. The timeout below is the
    // belt to this suspenders for any other format that misbehaves.
    // Candidate = not yet analysed *for the current model*. The sentinel value
    // holds the MODEL_ID it was stamped under, so a model bump (e.g.
    // clap-audio-2023 → clap-music-2023) makes every track a candidate again and
    // the sweep re-embeds the whole library into the new space, exactly once.
    let rows = match backend.query_many(
        "SELECT t.id, t.file_path FROM tracks t \
         WHERE t.file_path IS NOT NULL AND t.file_path != '' \
           AND (t.format IS NULL OR \
                lower(t.format) NOT IN ('dsd', 'dsf', 'dff', 'dsdiff')) \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'audio_embed_analyzed' \
                   AND m.value = ?) \
         LIMIT ?",
        &[
            &MODEL_ID as &dyn ToSqlValue,
            &(TRACK_BATCH as i64) as &dyn ToSqlValue,
        ],
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
        // A hard timeout guards against a decoder that spins on a pathological
        // file: on elapse we abandon the await (the blocking thread cannot be
        // cancelled, but one leaked thread is survivable) and move on, stamping
        // the sentinel below so the file is not retried.
        let p = path.clone();
        let decoded = tokio::time::timeout(
            std::time::Duration::from_secs(DECODE_TIMEOUT_SECS),
            tokio::task::spawn_blocking(move || {
                crate::audio::decode::decode_to_pcm(&p, Some(48_000), Some(1), 0.0, 10.0)
            }),
        )
        .await;
        if decoded.is_err() {
            warn!(track_id, path = %path, "audio_embed_decode_timeout");
        }

        if let Ok(Ok(Ok(d))) = decoded {
            let wav = to_mono_f32(&d.samples_i32, d.channels, d.bit_depth);
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

        // Stamp the sentinel with the current MODEL_ID whether or not it
        // produced a vector, so a broken or silent file drops out of the sweep
        // (until the next model bump) instead of being retried every pass.
        let _ = repo.set(track_id, SENTINEL, MODEL_ID);
        done += 1;
        // Relu à chaque fichier : baisser le débit pendant que l'analyse tourne
        // doit se sentir tout de suite, pas au prochain démarrage du serveur.
        let pause = per_file_pause_ms(&crate::db::settings_repo::SettingsRepo::with_backend(
            backend.clone(),
        ));
        if pause > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
        }
    }

    // Carry the memory figures on every batch. On 2026-08-10 the sweep's cost
    // had to be reconstructed after the fact from a 5-minute process-wide RSS
    // sampler, which is what let "memory rises while the pass runs" pass for a
    // cause (#1462). Now the pass states its own cost, batch by batch.
    info!(
        embedded = done,
        rss_mb = process_rss_mb().unwrap_or(0),
        available_mb = available_memory_mb().unwrap_or(0),
        "audio_embedding_batch"
    );
    done
}

/// Idle wait when disabled or the sweep is drained.
const IDLE_SLEEP_SECS: u64 = 900;
/// Wait between retries while the memory budget holds the sweep back. Long
/// enough not to spin, short enough that the sweep resumes on its own once
/// whatever was using the memory (a scan, a transcode) has finished.
const LOW_MEMORY_RETRY_SECS: u64 = 300;
/// Opt-in gate: only "true" enables it (heavy, needs the model downloaded).
const ENABLED_KEY: &str = "audio_embedding_enabled";
/// Where the model file goes; falls back to `TUNE_AUDIO_EMBED_MODEL`.
const MODEL_PATH_KEY: &str = "audio_embedding_model_path";
/// Published CLAP **music** audio tower (ONNX, mel in-graph, 512-d;
/// `music_audioset`/HTSAT-base). Fetched on first activation and cached at the
/// configured path. Supersedes the generalist `clap-audio-2023`.
const MODEL_URL: &str = "https://github.com/renesenses/tune-server-rust/releases/download/models/clap-music-2023/clap-audio-music-2023.onnx";
/// SHA-256 of the model above — a corrupt/partial download is rejected, and an
/// existing file whose hash differs (e.g. the old `clap-audio-2023` at the same
/// configured path) is treated as stale and re-fetched.
const MODEL_SHA256: &str = "d888118262b6144033928e5d7bed57a51bacde7899c4c4a109de1074857b951a";

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
    ensure_file(dest, MODEL_URL, MODEL_SHA256, "audio_model").await
}

/// Fetch `url` to `dest` unless a file with the expected SHA-256 is already
/// there. An existing file whose hash does NOT match `sha256` is treated as
/// stale (e.g. a superseded model at the same configured path) and re-fetched.
/// Written atomically via a temp file so a killed download never leaves a
/// truncated file in place.
pub(super) async fn ensure_file(
    dest: &Path,
    url: &str,
    sha256: &str,
    what: &str,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    if dest.exists() {
        match std::fs::read(dest) {
            Ok(existing) => {
                let have = format!("{:x}", Sha256::new_with_prefix(&existing).finalize());
                if have == sha256 {
                    return Ok(());
                }
                warn!(dest = %dest.display(), what, "stale_asset_rehashing_mismatch_refetch");
            }
            Err(e) => {
                warn!(dest = %dest.display(), what, error = %e, "asset_reread_failed_refetch")
            }
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    info!(url, dest = %dest.display(), what, "asset_downloading");
    let bytes = reqwest::get(url)
        .await
        .map_err(|e| format!("download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download status: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download body: {e}"))?;

    let got = format!("{:x}", Sha256::new_with_prefix(&bytes).finalize());
    if got != sha256 {
        return Err(format!("checksum mismatch: got {got}, want {sha256}"));
    }

    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("rename: {e}"))?;
    info!(dest = %dest.display(), bytes = bytes.len(), what, "asset_ready");
    Ok(())
}

/// Provision the onnxruntime shared lib next to the model and load it globally
/// into `ort` (once per process; see [`super::runtime::ensure_loaded`]). The
/// dylib is cached under the model's directory, so it is fetched only on the
/// first activation.
async fn ensure_runtime_loaded(model_path: &Path) -> Result<(), String> {
    let cache_root = model_path.parent().unwrap_or_else(|| Path::new("."));
    super::runtime::ensure_loaded(cache_root).await
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
        // Whether we are currently held back by the memory budget, so the
        // warning is logged on the way in and the recovery on the way out —
        // once each, not once per retry.
        let mut low_memory = false;
        // Same latch for "enabled but no model path": say it once, not every
        // 900 s round.
        let mut unconfigured = false;
        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            if enabled(&settings) {
                // Memory budget, checked BEFORE the model is fetched or the ORT
                // session built — those are themselves most of the footprint
                // (287 MB on disk, more once resident), so a box that cannot
                // afford the sweep must not download a third of a gigabyte and
                // build a session only to give up afterwards.
                //
                // The sweep is the heaviest thing this server does and it is
                // entirely optional: pausing it always beats being killed.
                // Re-checked every pass round, which is fine-grained enough to
                // react and coarse enough to be free.
                let avail = available_memory_mb();
                if let Some(mb) = avail {
                    if mb < MIN_AVAILABLE_MB {
                        // Log the transition, not every retry: this loop comes
                        // round every LOW_MEMORY_RETRY_SECS and a line each time
                        // would bury the journal on the very machine that is
                        // already struggling.
                        if !low_memory {
                            low_memory = true;
                            warn!(
                                available_mb = mb,
                                needed_mb = MIN_AVAILABLE_MB,
                                "audio_embed_paused_low_memory — acoustic analysis is paused until memory frees up; playback and the rest of the library are unaffected"
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(LOW_MEMORY_RETRY_SECS))
                            .await;
                        continue;
                    }
                }
                if low_memory {
                    low_memory = false;
                    info!(
                        available_mb = avail.unwrap_or(0),
                        "audio_embed_resumed_memory_ok"
                    );
                }

                if embedder.is_none() {
                    // No model path configured: `enabled=true` and yet the sweep
                    // can do nothing at all. Before this, that branch fell
                    // through in complete silence — no batch, no error, just the
                    // 900 s idle sleep — which reads exactly like a sweep that
                    // has finished. Lived on .18 (2026-08-11): the rebuilt
                    // database had lost `audio_embedding_model_path`, the
                    // feature was on, and the journal said nothing whatsoever
                    // for twelve minutes.
                    //
                    // Latched like the memory pause: once on the way in, once on
                    // the way out. This loop comes round every 900 s and a line
                    // per round would be noise.
                    let model_path = configured_model_path(&settings);
                    if model_path.is_none() {
                        if !unconfigured {
                            unconfigured = true;
                            warn!(
                                setting = MODEL_PATH_KEY,
                                env = "TUNE_AUDIO_EMBED_MODEL",
                                "audio_embed_no_model_path — acoustic analysis is enabled but no model path is configured, so nothing will be analysed; set the setting or the environment variable"
                            );
                        }
                    } else if unconfigured {
                        unconfigured = false;
                        info!("audio_embed_model_path_configured");
                    }
                    if let Some(p) = model_path {
                        if let Err(e) = ensure_model(&p).await {
                            warn!(error = %e, "audio_model_unavailable");
                        } else if let Err(e) = ensure_runtime_loaded(&p).await {
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

    #[test]
    fn to_mono_f32_averages_interleaved_stereo() {
        // 16-bit full scale = 2^15 = 32768. Two stereo frames.
        let s = [16384, 0, 0, -16384];
        let m = to_mono_f32(&s, 2, 16);
        assert_eq!(m.len(), 2);
        assert!((m[0] - 0.25).abs() < 1e-4, "{}", m[0]); // (16384+0)/2 / 32768
        assert!((m[1] + 0.25).abs() < 1e-4, "{}", m[1]); // (0-16384)/2 / 32768
    }

    #[test]
    fn to_mono_f32_mono_passthrough() {
        let m = to_mono_f32(&[16384, -16384], 1, 16);
        assert_eq!(m.len(), 2);
        assert!((m[0] - 0.5).abs() < 1e-4);
        assert!((m[1] + 0.5).abs() < 1e-4);
    }

    #[test]
    fn cgroup_cap_reports_headroom() {
        // 1 GiB cap, 256 MiB used → 768 MiB left. This is the container case
        // that /proc/meminfo gets wrong: the host may have 32 GB free while
        // this process has 768 MB before the OOM killer arrives.
        let mb = super::cgroup_available_mb(Some("1073741824\n"), Some("268435456\n"));
        assert_eq!(mb, Some(768));
    }

    #[test]
    fn cgroup_unlimited_falls_through() {
        // cgroup v2 writes the literal "max"...
        assert_eq!(super::cgroup_available_mb(Some("max\n"), Some("100")), None);
        // ...and v1 an i64::MAX-ish sentinel. Neither is a real cap, so both
        // must yield None and let the caller read the host instead.
        assert_eq!(
            super::cgroup_available_mb(Some("9223372036854771712"), Some("100")),
            None
        );
    }

    #[test]
    fn cgroup_usage_over_limit_saturates_to_zero() {
        // Usage can exceed the limit momentarily under reclaim pressure. That
        // must read as "no headroom", not wrap around to a huge number and let
        // the sweep charge ahead.
        assert_eq!(
            super::cgroup_available_mb(Some("104857600"), Some("209715200")),
            Some(0)
        );
    }

    #[test]
    fn mem_available_is_read_not_mem_free() {
        let meminfo = "MemTotal:       16316360 kB\n\
                       MemFree:          201480 kB\n\
                       MemAvailable:   14765112 kB\n\
                       Buffers:           21356 kB\n";
        // 14765112 kB = 14419 MB — and emphatically not MemFree's 196 MB, which
        // would pause the sweep permanently on any long-running server.
        assert_eq!(super::mem_available_mb(meminfo), Some(14419));
    }

    #[test]
    fn missing_mem_available_is_unknown_not_zero() {
        // An old kernel without MemAvailable must read as "cannot tell", so the
        // guard stands aside. Zero would pause the sweep forever.
        assert_eq!(super::mem_available_mb("MemTotal: 100 kB\n"), None);
    }

    #[test]
    fn observed_18_footprint_clears_the_budget() {
        // .18 on 2026-08-10: 16 GB box, sweep running, RSS peaked at 1224 MB and
        // MemAvailable stayed around 14.7 GB. The guard must not fire there —
        // it exists for the 2 GB Pi, not for the machine where the footprint
        // was measured.
        let meminfo = "MemAvailable:   14765112 kB\n";
        assert!(super::mem_available_mb(meminfo).unwrap() > super::MIN_AVAILABLE_MB);
        // A 2 GB box with ~700 MB left is exactly the case that must pause.
        assert!(
            super::mem_available_mb("MemAvailable: 716800 kB\n").unwrap() < super::MIN_AVAILABLE_MB
        );
    }

    #[test]
    fn quantize_i16_matches_int16_roundtrip() {
        // Exact grid points survive unchanged.
        assert_eq!(quantize_i16(0.0), 0.0);
        assert!((quantize_i16(1.0) - 1.0).abs() < 1e-6); // 32767/32767
        assert!((quantize_i16(-1.0) + 1.0).abs() < 1e-6);
        // Out-of-range clamps before scaling (no i16 wrap).
        assert!((quantize_i16(2.0) - 1.0).abs() < 1e-6);
        assert!((quantize_i16(-2.0) + 1.0).abs() < 1e-6);
        // A 24-bit-resolution value snaps to the nearest 16-bit step (truncation
        // toward zero): 0.5 * 32767 = 16383.5 → 16383 → /32767.
        let q = quantize_i16(0.5);
        assert!((q - 16383.0 / 32767.0).abs() < 1e-6, "{q}");
        // Sub-LSB positive values collapse to 0 (truncation), proving 16-bit grid.
        assert_eq!(quantize_i16(1.0 / 65536.0), 0.0);
    }

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
        let model = dir.join("clap-audio-music-2023.onnx");

        ensure_model(&model).await.expect("download model");
        ensure_runtime_loaded(&model)
            .await
            .expect("provision onnxruntime");

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
