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
/// Ce réglage tient DEUX leviers, et le second a été ajouté après coup parce que
/// le premier ne suffisait pas :
/// - [`per_file_pause_ms`] : la pause entre deux fichiers. Elle laisse la main
///   au reste du serveur sans jamais rien interrompre en cours de route.
/// - [`intra_threads_for`] : le nombre de fils qu'onnxruntime a le droit
///   d'utiliser *pendant* une inférence.
///
/// La passe est séquentielle fichier par fichier, mais **une inférence ne l'est
/// pas** : sans plafond, onnxruntime prend tous les cœurs. La pause seule
/// laissait donc la machine à genoux entre deux respirations.
const THROTTLE_KEY: &str = "audio_embedding_throttle";

/// Combien de fils onnxruntime a le droit d'utiliser à l'intérieur d'une
/// inférence, selon le même réglage que la pause entre fichiers.
///
/// **La pause ne bride pas ce qui chauffe.** `per_file_pause_ms` espace les
/// fichiers ; pendant chaque inférence, onnxruntime prend par défaut *tous* les
/// cœurs. Sur .18 (8 cœurs) une salve de 32 fichiers dure ~53 s pour 50 ms de
/// pause par fichier : le rapport cyclique est de ~97 %. `sar` a mesuré 50 % de
/// 8 cœurs en continu pendant 1 h 25, l'analyse acoustique et ReplayGain
/// tournant ensemble, avant que la machine ne s'arrête net — journal interrompu
/// en pleine ligne, aucune trace noyau, mémoire hors de cause (14 Go libres).
/// La cause exacte de cet arrêt n'est pas établie ; ce qui l'est, c'est qu'une
/// analyse optionnelle de fond n'a aucune raison de prendre la machine entière.
///
/// - `eco` : un seul fil. La machine reste entièrement disponible.
/// - `equilibre` (défaut) : la moitié des cœurs, au moins un. Laisse de quoi
///   décoder, servir un flux et répondre à l'interface.
/// - `rapide` : tous les cœurs — le comportement d'avant, désormais un choix
///   explicite et non plus le défaut silencieux.
/// Réglage de débit par défaut, choisi d'après la taille de la machine.
///
/// `equilibre` (la moitié des cœurs) était le défaut universel : sur .18
/// (8 cœurs) cela fait 4 fils ONNX qui, avec ReplayGain en parallèle, ont
/// éteint la machine deux fois (#1576). Le matériel typique d'un serveur
/// audio — Pi, NAS, mini-PC — est précisément dans cette gamme, et personne
/// n'ira changer un réglage qu'il ne sait pas exister. Au-delà de huit cœurs,
/// la machine a de quoi encaisser et `equilibre` reste le bon compromis.
fn default_throttle() -> &'static str {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    if cores <= 8 { "eco" } else { "equilibre" }
}

fn intra_threads_for(settings: &crate::db::settings_repo::SettingsRepo) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let default = default_throttle();
    let setting = settings.get(THROTTLE_KEY).ok().flatten();
    match setting.as_deref().unwrap_or(default) {
        "eco" => 1,
        "rapide" => cores,
        // Comme pour la pause : une valeur inconnue retombe sur l'équilibre.
        _ => (cores / 2).max(1),
    }
}

/// Pause entre deux fichiers selon le réglage. `eco` divise la charge par huit
/// environ, `rapide` enchaîne sans pause — à réserver à une machine dédiée ou à
/// une nuit d'analyse.
fn per_file_pause_ms(settings: &crate::db::settings_repo::SettingsRepo) -> u64 {
    let default = default_throttle();
    let setting = settings.get(THROTTLE_KEY).ok().flatten();
    match setting.as_deref().unwrap_or(default) {
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
    /// `intra_threads` caps how many threads onnxruntime may use *inside* a
    /// single inference. See [`intra_threads_for`] for why that matters more
    /// than the pause between files.
    pub fn load(model_path: &Path, intra_threads: usize) -> Result<Self, String> {
        let session = Session::builder()
            .map_err(|e| format!("ort builder: {e}"))?
            .with_intra_threads(intra_threads)
            .map_err(|e| format!("ort intra_threads({intra_threads}): {e}"))?
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
/// Cadence d'entrée du front-end mel de CLAP. `decode_to_pcm` la *demande*,
/// mais le chemin symphonia ne rééchantillonne pas : depuis #1508 il dit la
/// vérité (`d.sample_rate` = cadence source) au lieu de mentir, et c'est donc
/// ici que la fenêtre doit être amenée réellement à 48 kHz.
const CLAP_INPUT_RATE: u32 = 48_000;

/// La fenêtre exactement comme le modèle l'attend : mono (vraie moyenne des
/// canaux, #1108/#1508) puis VRAI 48 kHz (rubato). Avant ce correctif (#1498),
/// un FLAC 44,1 stéréo arrivait au modèle en L/R entrelacé étiqueté mono
/// 48 kHz : ~5,4 s de musique au lieu de 10, timbre brouillé, et des vecteurs
/// dépendant de l'encodage — rédhibitoire pour toute mutualisation.
fn prepare_clap_window(
    samples: &[i32],
    channels: u32,
    bit_depth: u16,
    sample_rate: u32,
) -> Vec<f32> {
    let mono = to_mono_f32(samples, channels, bit_depth);
    if sample_rate == CLAP_INPUT_RATE || sample_rate == 0 || mono.is_empty() {
        return mono;
    }
    // `_exact` : sans elle, la sortie porte le retard de groupe du filtre en
    // tête et sa vidange en queue (~2× le délai) — du silence et un décalage
    // que le modèle n'a pas à voir.
    crate::audio::resample::rubato_resample_batch_exact(&mono, sample_rate, CLAP_INPUT_RATE, 1)
}

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
        // Playback can start mid-batch; yield at once so neither the decode
        // nor the inference competes with the audio pipeline (#1515) — the
        // same mid-batch bail as the ReplayGain pass (#1310).
        if crate::audio::replaygain::any_zone_playing(backend) {
            info!("audio_embed_yield_to_playback — zone playing, pausing sweep mid-batch");
            break;
        }
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
            let wav = prepare_clap_window(&d.samples_i32, d.channels, d.bit_depth, d.sample_rate);
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
/// Attente entre deux vérifications quand la machine est trop chaude. Assez
/// long pour lui laisser le temps de redescendre, assez court pour reprendre
/// d'elle-même sans intervention (#1576).
const THERMAL_RETRY_SECS: u64 = 120;
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
/// Répertoire par défaut du modèle, quand ni le réglage ni la variable
/// d'environnement ne disent où le mettre. Même répertoire que le modèle
/// **texte** (`text_paths`), pour qu'audio et texte partagent la dylib
/// onnxruntime au lieu de la télécharger deux fois. Chemin relatif, comme
/// `artwork_cache` : résolu depuis le répertoire de travail du serveur.
const DEFAULT_MODEL_DIR: &str = "embedding_models";
/// Nom de fichier par défaut — celui de l'archive publiée.
const DEFAULT_MODEL_FILE: &str = "clap-audio-music-2023.onnx";

fn enabled(settings: &crate::db::settings_repo::SettingsRepo) -> bool {
    settings.get(ENABLED_KEY).ok().flatten().as_deref() == Some("true")
}

/// The configured model destination (setting → env). Not required to exist —
/// [`ensure_model`] downloads it there on first run.
/// La passe peut-elle réellement travailler ? Autrement dit : un chemin de
/// modèle est-il configuré, et le fichier est-il présent ?
///
/// Sans ça, l'interface ne pouvait pas distinguer « analyse en cours, au
/// début » de « analyse activée mais incapable de démarrer » — les deux
/// donnaient 0 %. Fabien, v0.9.68 : « Analyse en cours bloqué à 0 % ». Ses
/// journaux ne contenaient aucune ligne d'analyse audio : la passe n'avait
/// jamais tourné, et la jauge affichait quand même une progression.
///
/// `false` ne veut pas dire « cassé » : au tout premier démarrage le modèle
/// n'est pas encore téléchargé, et la passe le récupère d'elle-même. Mais
/// l'interface doit pouvoir le DIRE plutôt que de montrer une barre figée.
pub fn model_ready(settings: &crate::db::settings_repo::SettingsRepo) -> bool {
    configured_model_path(settings).exists()
}

/// Emplacement du modèle : réglage, puis variable d'environnement, puis un
/// défaut.
///
/// Le défaut est indispensable, pas cosmétique. Tant que cette fonction pouvait
/// répondre `None`, activer l'analyse acoustique sur une installation où
/// `audio_embedding_model_path` n'avait jamais été écrit ne téléchargeait
/// **rien** : la passe voyait `None`, sautait le bloc entier et repartait
/// dormir — sans téléchargement, sans avertissement, sans la moindre ligne de
/// journal. Le réglage semblait actif et il ne se passait rien.
///
/// Le côté **texte** avait déjà rencontré la panne et l'avait réglée ainsi
/// (`text_paths`, repli `embedding_models/`) après le #1288 de Fabien
/// (« Menu Ambiance → 503 », `audio_embedding_model_path unset`). La passe
/// audio, elle, était restée avec son `None`. On aligne les deux : même
/// répertoire, même convention relative que `artwork_cache`, résolue depuis le
/// répertoire de travail du serveur.
fn configured_model_path(settings: &crate::db::settings_repo::SettingsRepo) -> PathBuf {
    settings
        .get(MODEL_PATH_KEY)
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_AUDIO_EMBED_MODEL").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MODEL_DIR).join(DEFAULT_MODEL_FILE))
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
pub fn spawn(backend: Arc<dyn DbBackend>, license: Arc<crate::license::LicenseManager>) {
    use crate::db::settings_repo::SettingsRepo;
    tokio::spawn(async move {
        // Let startup/scan settle before touching the disk hard.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut embedder: Option<AudioEmbedder> = None;
        // Whether we are currently held back by the memory budget, so the
        // warning is logged on the way in and the recovery on the way out —
        // once each, not once per retry.
        let mut low_memory = false;
        // Loquet « licence non premium » : une ligne à l'entrée, une à la
        // sortie. Cette boucle repasse toutes les 900 s et une ligne par tour
        // noierait le journal.
        let mut not_premium = false;
        // Le nombre de fils avec lequel la session courante a été bâtie, pour
        // détecter un changement de réglage.
        let mut loaded_threads = 0usize;
        // Garde thermique de CETTE passe : il porte sa propre hystérésis et
        // journalise ses propres transitions (#1576).
        let mut thermal = crate::audio::thermal::ThermalGate::new();
        // Latch for the playback hold, same style as `low_memory`: one line on
        // the way in, one on the way out, silence in between.
        let mut playback_hold = false;
        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            // Garde premium. Vérifié à CHAQUE tour, et non une seule fois au
            // démarrage : une clé posée ou retirée doit prendre effet sans
            // redémarrer le serveur — c'est ce que fait déjà la revalidation
            // périodique côté licence.
            //
            // Il n'y avait aucun contrôle jusqu'ici. Constaté sur .18 le
            // 2026-08-13 : `tier: free`, aucune clé, et pourtant
            // `audio_embedding_batch embedded=10 rss_mb=1816` — l'analyse
            // acoustique, la chose la plus lourde que fasse ce serveur,
            // tournait sur une installation gratuite.
            if enabled(&settings)
                && !license
                    .check_feature(crate::license::Feature::AcousticAnalysis)
                    .await
            {
                if !not_premium {
                    not_premium = true;
                    info!(
                        "audio_embed_requires_premium — l'analyse acoustique est réservée au premium ; le réglage reste actif et la passe reprendra dès qu'une licence sera validée"
                    );
                }
                // On relâche aussi la session ONNX : inutile de garder ~300 Mo
                // résidents pour une passe qui ne tournera pas.
                embedder = None;
                tokio::time::sleep(std::time::Duration::from_secs(LOW_MEMORY_RETRY_SECS)).await;
                continue;
            }
            if not_premium {
                not_premium = false;
                info!("audio_embed_premium_ok — licence validée, l'analyse acoustique reprend");
            }
            if enabled(&settings) {
                // Yield to playback, like the ReplayGain pass (#1310) — this
                // sweep was the only analysis without the guard (#1515). It
                // decodes audio AND runs multi-threaded ONNX inference: left
                // running during an OAAT session on .18 (2026-08-12) it held
                // the server at ~380 % CPU, the WS event_bus lagged by
                // thousands of messages and the output pacing jittered into
                // audible micro-dropouts at the endpoint.
                if crate::audio::replaygain::any_zone_playing(&backend) {
                    if !playback_hold {
                        playback_hold = true;
                        info!(
                            "audio_embed_yield_to_playback — zone playing, acoustic analysis paused until playback stops"
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(
                        crate::audio::replaygain::PLAYBACK_BACKOFF_SECS,
                    ))
                    .await;
                    continue;
                }
                if playback_hold {
                    playback_hold = false;
                    info!("audio_embed_resumed_playback_stopped");
                }

                // Garde thermique (#1576) : avant toute dépense. Comme pour
                // la mémoire, une analyse facultative ne doit jamais mettre la
                // machine en danger — et ici le danger est physique.
                if thermal.should_hold("acoustique") {
                    tokio::time::sleep(std::time::Duration::from_secs(THERMAL_RETRY_SECS)).await;
                    continue;
                }

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
                if let Some(mb) = avail
                    && mb < MIN_AVAILABLE_MB
                {
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
                    tokio::time::sleep(std::time::Duration::from_secs(LOW_MEMORY_RETRY_SECS)).await;
                    continue;
                }
                if low_memory {
                    low_memory = false;
                    info!(
                        available_mb = avail.unwrap_or(0),
                        "audio_embed_resumed_memory_ok"
                    );
                }

                // Le nombre de fils est figé à la construction de la session.
                // Si le réglage a changé depuis, on relâche la session pour
                // qu'elle soit rebâtie juste en dessous — sinon le bouton de
                // l'interface ne ferait rien tant que le serveur n'a pas
                // redémarré, ce qui est précisément le genre de réglage qui
                // ment.
                let threads = intra_threads_for(&settings);
                if embedder.is_some() && threads != loaded_threads {
                    info!(
                        from = loaded_threads,
                        to = threads,
                        "audio_embed_threads_changed_reloading"
                    );
                    embedder = None;
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
                    // Plus de branche « aucun chemin configuré » : le chemin est
                    // désormais toujours résolu, avec un défaut. Avant, activer
                    // l'analyse sur une installation dont le réglage n'avait
                    // jamais été écrit ne téléchargeait RIEN — la passe voyait
                    // `None` et repartait dormir. Elle le signalait, ce qui
                    // valait mieux que le silence, mais un avertissement n'a
                    // jamais téléchargé un modèle.
                    let p = configured_model_path(&settings);
                    if let Err(e) = ensure_model(&p).await {
                        warn!(error = %e, path = %p.display(), "audio_model_unavailable");
                    } else if let Err(e) = ensure_runtime_loaded(&p).await {
                        warn!(error = %e, "audio_runtime_unavailable");
                    } else {
                        match AudioEmbedder::load(&p, threads) {
                            Ok(e) => {
                                info!(
                                    model = %p.display(),
                                    intra_threads = threads,
                                    "audio_embedder_loaded"
                                );
                                embedder = Some(e);
                                loaded_threads = threads;
                            }
                            Err(e) => warn!(error = %e, "audio_embedder_load_failed"),
                        }
                    }
                }
                if let Some(emb) = embedder.as_mut() {
                    // Une passe lourde à la fois (#1576) : si ReplayGain
                    // décode, on attend notre tour — les deux ensemble ont
                    // déjà éteint une machine.
                    let did = {
                        let _slot = crate::audio::replaygain::ANALYSIS_SLOT.lock().await;
                        analyze_embedding_batch(&backend, emb).await
                    };
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
    fn clap_window_resamples_true_source_rate_to_48k() {
        // 1 s of 44.1 kHz mono must come out ~1 s of 48 kHz (#1498).
        let samples: Vec<i32> = (0..44_100).map(|i| ((i % 100) * 100) as i32).collect();
        let out = prepare_clap_window(&samples, 1, 16, 44_100);
        let ratio = out.len() as f64 / 48_000.0;
        assert!((0.98..1.02).contains(&ratio), "got {} samples", out.len());
    }

    #[test]
    fn clap_window_passthrough_at_48k_and_downmixes() {
        // Already 48 kHz: untouched. Stereo: averaged first (#1108/#1508).
        let stereo: Vec<i32> = vec![32767, -32767, 32767, -32767];
        let out = prepare_clap_window(&stereo, 2, 16, 48_000);
        assert_eq!(out.len(), 2);
        assert!(
            out.iter().all(|v| v.abs() < 1e-4),
            "L/R must cancel out: {out:?}"
        );
    }

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

    fn settings_with_throttle(v: Option<&str>) -> crate::db::settings_repo::SettingsRepo {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let repo = crate::db::settings_repo::SettingsRepo::with_backend(std::sync::Arc::new(db));
        if let Some(v) = v {
            repo.set(super::THROTTLE_KEY, v).unwrap();
        }
        repo
    }

    fn cores() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
    }

    #[test]
    fn eco_uses_a_single_thread() {
        // Le point du mode éco : la machine reste entièrement disponible.
        assert_eq!(
            super::intra_threads_for(&settings_with_throttle(Some("eco"))),
            1
        );
    }

    #[test]
    fn fast_uses_every_core() {
        // L'ancien comportement, désormais un choix explicite.
        assert_eq!(
            super::intra_threads_for(&settings_with_throttle(Some("rapide"))),
            cores()
        );
    }

    #[test]
    fn default_leaves_half_the_machine_free() {
        // Ni réglage écrit, ni valeur reconnue : la moitié des cœurs. C'est ce
        // qui laisse de quoi décoder et servir un flux pendant l'analyse.
        let expected = (cores() / 2).max(1);
        assert_eq!(
            super::intra_threads_for(&settings_with_throttle(None)),
            expected
        );
        assert_eq!(
            super::intra_threads_for(&settings_with_throttle(Some("n'importe quoi"))),
            expected
        );
    }

    #[test]
    fn never_returns_zero() {
        // Zéro signifie « débrouille-toi » pour onnxruntime, donc tous les
        // cœurs — l'exact inverse de ce qu'on demande. Sur une machine à un
        // seul cœur, la division doit tenir.
        for v in [None, Some("eco"), Some("rapide"), Some("equilibre")] {
            assert!(
                super::intra_threads_for(&settings_with_throttle(v)) >= 1,
                "{v:?}"
            );
        }
    }

    #[test]
    fn model_path_falls_back_to_a_default_instead_of_nothing() {
        // Le défaut n'est pas cosmétique : tant que cette résolution pouvait
        // ne rien rendre, activer l'analyse acoustique sur une installation
        // dont `audio_embedding_model_path` n'avait jamais été écrit ne
        // téléchargeait RIEN — la passe sautait le bloc entier et repartait
        // dormir. Ce test échoue contre l'ancien code, qui rendait `None`.
        //
        // On vérifie le répertoire ET le nom : le répertoire doit rester le
        // même que celui du modèle texte (`text_paths`), pour que les deux
        // partagent la dylib onnxruntime au lieu de la télécharger deux fois.
        let p = super::configured_model_path(&settings_with_throttle(None));
        assert_eq!(
            p.parent().and_then(|d| d.file_name()),
            Some(std::ffi::OsStr::new(super::DEFAULT_MODEL_DIR)),
            "le défaut doit viser {} — partagé avec le modèle texte",
            super::DEFAULT_MODEL_DIR
        );
        assert_eq!(
            p.file_name(),
            Some(std::ffi::OsStr::new(super::DEFAULT_MODEL_FILE))
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

        let mut embedder = AudioEmbedder::load(&model, 1).expect("load embedder");
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
