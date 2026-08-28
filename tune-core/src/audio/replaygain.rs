//! Background ReplayGain analysis.
//!
//! The scan reads ReplayGain tags straight from the file (fast, no decode) but
//! most files have none. This pass FILLS the missing values by measuring EBU
//! R128 loudness — which requires decoding the whole file, far too expensive to
//! do inline in the scan (58k-file libraries already draw "scan interminable"
//! complaints). So it runs as a throttled, resumable background task, entirely
//! separate from the scan walk: the scan stays tag-only and fast, the heavy
//! calculation lives here.
//!
//! Written to `track_metadata` as `rg_track_gain` / `rg_track_peak` (+ album
//! variants), matching the keys `metadata::read_extended_metadata` uses for
//! file-tag ReplayGain — so the two are interchangeable downstream. A file's own
//! ReplayGain tags always win: a track that already has `rg_track_gain` is never
//! recomputed.

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{debug, info, warn};

/// ReplayGain 2.0 reference loudness. `track_gain = REFERENCE_LUFS - measured`.
pub const REFERENCE_LUFS: f64 = -18.0;

/// Tracks analysed per wake-up before the loop sleeps again. Small so the pass
/// never monopolises the CPU on a big library — it chips away over time.
const TRACK_BATCH: usize = 25;

/// Pause between per-file analyses (each one fully decodes a track). Keeps the
/// pass "nice": it must never compete with playback or make the machine hot.
const PER_FILE_PAUSE_MS: u64 = 400;

/// How long the loop sleeps once there is nothing left to analyse.
const IDLE_SLEEP_SECS: u64 = 900;

/// Ceiling on the ESTIMATED decoded footprint of one track before analysis.
///
/// `measure_loudness_and_peak` holds the whole track twice: the decoder's
/// `Vec<i32>` (4 B/sample) plus the normalised `Vec<f64>` (8 B/sample) —
/// 12 bytes per sample overall. A 30-minute 24/192 stereo track is ~690 M
/// samples ≈ 8 GB: on a 6-7 GB box the OOM killer shoots the whole server
/// in a loop (#1109, production 02/08). Until the analysis streams, any
/// track whose estimate exceeds this budget is skipped (stamped, logged) —
/// the overwhelming majority of libraries stays fully analysed.
const MAX_ANALYSIS_EST_BYTES: u64 = 1_200_000_000;

/// A single file must never stall the whole sweep. `measure_loudness_and_peak`
/// decodes in segments via `spawn_blocking`; a pathological file (corrupt FLAC,
/// symphonia decode loop) or a dormant NAS mount can make a segment hang and
/// never return — the pass then gets stuck on that one file forever
/// (« n'avance plus », Bilou #1155). Bound each track: on timeout we log, stamp
/// it analysed and move on. Generous vs. a normal streaming analysis (seconds,
/// up to ~2 min for a very long hi-res track), tight vs. an indefinite hang.
const PER_TRACK_ANALYSIS_TIMEOUT_SECS: u64 = 180;

/// Attente entre deux vérifications quand la machine est trop chaude (#1576).
const THERMAL_RETRY_SECS: u64 = 120;

/// How long the sweep backs off after finding a zone actively playing. The
/// track pass fully decodes files — often over a network (SMB/NAS) mount — and
/// on a busy link that starves the same disk/network the player reads from,
/// stalling the audio pipeline (#1310, « la musique s'arrête au premier
/// morceau »). The pass yields entirely while anything plays, then rechecks.
/// Shared with the audio-embedding sweep (#1515), which obeys the same rule.
pub(crate) const PLAYBACK_BACKOFF_SECS: u64 = 30;

/// The 12 B/sample estimate above, from the DB columns the scan filled.
/// Unknown rate/channels fall back to CD stereo; unknown duration returns 0
/// (no basis to refuse — the track is analysed as before).
/// Au-dela de cette frequence, la valeur stockee ne peut pas etre une cadence
/// PCM : le maximum rencontre en PCM est 768 kHz. C'est donc une cadence DSD
/// brute (2,8 MHz pour du DSD64, 11,3 MHz pour du DSD256), et l'analyse ne
/// decode pas a cette cadence-la.
const MAX_PLAUSIBLE_PCM_RATE: i64 = 768_000;

/// Cadence a laquelle l'analyse decodera reellement ce fichier.
///
/// `tracks.sample_rate` contient, pour du DSD, la cadence DSD BRUTE. La prendre
/// pour une cadence PCM surestimait l'empreinte memoire d'un facteur 16 a 32 :
/// le fichier DSD256 de Cyrille (#1330) etait annonce a 100 Go pour 6 minutes
/// de musique, la ou le decodage PCM en represente 3. Consequence, tout fichier
/// DSD etait ecarte de l'analyse comme « surdimensionne », y compris ceux qui
/// tiennent largement dans le budget.
fn effective_decode_rate(sample_rate: i64) -> i64 {
    if sample_rate > MAX_PLAUSIBLE_PCM_RATE {
        crate::audio::formats::AudioFormat::Dsd.dsd_output_sample_rate(sample_rate as u32) as i64
    } else {
        sample_rate
    }
}

fn estimated_analysis_bytes(
    duration_ms: Option<i64>,
    sample_rate: Option<i64>,
    channels: Option<i64>,
) -> u64 {
    let dur_s = match duration_ms {
        Some(ms) if ms > 0 => ms as u64 / 1000,
        _ => return 0,
    };
    let rate = sample_rate
        .filter(|&r| r > 0)
        .map(effective_decode_rate)
        .unwrap_or(44_100) as u64;
    let ch = channels.filter(|&c| c > 0).unwrap_or(2) as u64;
    dur_s
        .saturating_mul(rate)
        .saturating_mul(ch)
        .saturating_mul(12)
}

/// Setting gate. Absent/"true" ⇒ on (Bertrand wants calculated tags filled
/// automatically); set to "false" to disable the whole pass.
const ENABLED_KEY: &str = "replaygain_analysis_enabled";

/// Format a gain the way ReplayGain tags do, e.g. `-6.50 dB`.
pub fn format_gain(db: f64) -> String {
    format!("{:.2} dB", db)
}

/// Format a linear peak (0.0–1.0), e.g. `0.988553`.
pub fn format_peak(peak: f64) -> String {
    format!("{:.6}", peak)
}

/// `track_gain = REFERENCE_LUFS - measured_lufs`.
pub fn track_gain_db(lufs: f64) -> f64 {
    REFERENCE_LUFS - lufs
}

fn enabled(settings: &SettingsRepo) -> bool {
    settings
        .get(ENABLED_KEY)
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true)
}

/// Verrou GLOBAL des analyses lourdes : UNE seule passe décode à la fois.
///
/// ReplayGain et le sweep acoustique décodent tous deux des fichiers entiers ;
/// ensemble ils ont tenu .18 à ~450 % CPU pendant 75 minutes avant que la
/// machine ne s'éteigne net (#1576, 2e arrêt de ce type — journal coupé en
/// pleine ligne). Chaque passe reste bornée et cède déjà à la lecture ; ce
/// verrou fait qu'elles se succèdent au lieu de s'additionner : le pic de
/// charge est divisé par deux, la progression totale est identique.
///
/// Particulièrement important au premier démarrage après une mise à jour qui
/// invalide les deux analyses à la fois (échelle SACD #1638 → RG des DSD,
/// bump de modèle #1498 → tous les embeddings) : sans lui, tout le parc
/// rejouerait le scénario du crash.
pub(crate) static ANALYSIS_SLOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// True if any zone is currently playing, per the persisted `last_play_state`
/// the orchestrator writes on every play/pause/stop. The ReplayGain track pass
/// must yield to playback (#1310): decoding whole files — often over a network
/// mount — otherwise saturates the same disk/network the player reads from and
/// stalls audio. Fails open (returns `false`) on a query error so a DB hiccup
/// can never freeze the sweep permanently. Shared with the audio-embedding
/// sweep (#1515), which must yield for the same reason and then some: its
/// batches also run multi-threaded ONNX inference on top of the decode.
pub(crate) fn any_zone_playing(backend: &Arc<dyn DbBackend>) -> bool {
    playing_zone_name(backend).is_some()
}

/// Le NOM de la zone qui joue, pour que le report des passes d'analyse soit
/// diagnosticable.
///
/// `any_zone_playing` ne disait que « oui » ou « non », et les journaux se
/// bornaient à « pausing sweep ». Face à une analyse figée alors qu'il ne jouait
/// rien, l'utilisateur ne pouvait pas savoir QUELLE zone la retenait — trois
/// signalements ont buté là-dessus (#1464, #1456, #1457), la cause étant une
/// zone restée à `playing` après un arrêt brutal. Nommer la zone rend la cause
/// lisible dans le journal, sans lire le code.
pub(crate) fn playing_zone_name(backend: &Arc<dyn DbBackend>) -> Option<String> {
    backend
        .query_one(
            "SELECT name FROM zones WHERE last_play_state = 'playing' LIMIT 1",
            &[],
        )
        .ok()
        .flatten()
        .map(|cols| {
            cols.first()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "?".to_string())
        })
}

/// Spawn the background ReplayGain analysis loop. Drains tracks that lack
/// ReplayGain, then idles; picks up any new tracks after later scans on its own.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    tokio::spawn(async move {
        // Let startup/scan settle before touching the disk hard.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        // Garde thermique de cette passe (#1576) : ReplayGain décode des
        // fichiers entiers, c'est l'autre moitié de la charge qui a éteint .18.
        let mut thermal = crate::audio::thermal::ThermalGate::new();

        // ─── Registre des exécutions automatisées (#2080) ────────────────
        //
        // L'unité inscrite n'est PAS le lot : cette boucle en enchaîne des
        // centaines, et cinquante lignes de registre seraient consommées en
        // quelques minutes sans jamais raconter autre chose que « ça avance ».
        // L'unité est la CAMPAGNE — de la première piste trouvée jusqu'au
        // retour au repos. C'est la phrase que l'utilisateur attend : « la
        // passe a tourné de 21h04 à 21h37 et a analysé 812 pistes ».
        //
        // Le repos sans campagne ouverte est inscrit AUSSI, une seule fois par
        // transition (`repos_deja_inscrit`). C'est la réponse littérale à « ça
        // n'a rien fait » : la passe a tourné, elle n'a rien trouvé. Sans
        // cette ligne, une bibliothèque déjà entièrement analysée serait
        // indistinguable d'une passe jamais lancée. Sans le drapeau, la même
        // ligne se réécrirait toutes les IDLE_SLEEP_SECS et chasserait tout
        // l'historique utile hors de la rétention.
        let registre = crate::db::task_run_repo::TaskRunRepo::with_backend(backend.clone());
        let mut campagne: Option<crate::db::task_run_repo::Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos_deja_inscrit = false;

        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            if enabled(&settings) {
                if thermal.should_hold("replaygain") {
                    tokio::time::sleep(std::time::Duration::from_secs(THERMAL_RETRY_SECS)).await;
                    continue;
                }
                // Yield the decode-heavy track pass to playback (#1310). The
                // album pass is pure DB math (no file decode), so it keeps
                // making progress even while a zone plays.
                let playing = any_zone_playing(&backend);
                let did = if playing {
                    0
                } else {
                    {
                        // Une passe à la fois (#1576) : si le sweep acoustique
                        // décode, on attend notre tour plutôt que d'empiler.
                        let _slot = ANALYSIS_SLOT.lock().await;
                        analyze_track_batch(&backend).await
                    }
                };
                let albums = analyze_album_batch(&backend);

                if did > 0 || albums > 0 {
                    // Du travail : ouvrir la campagne si elle ne l'est pas déjà.
                    if campagne.is_none() {
                        campagne =
                            Some(registre.ouvrir(crate::db::task_run_repo::TACHE_REPLAYGAIN));
                        analysees = 0;
                    }
                    analysees += did as i64 + albums as i64;
                    repos_deja_inscrit = false;
                }

                if playing {
                    tokio::time::sleep(std::time::Duration::from_secs(PLAYBACK_BACKOFF_SECS)).await;
                } else if did == 0 && albums == 0 {
                    clore_campagne(
                        &registre,
                        &mut campagne,
                        &mut analysees,
                        &mut repos_deja_inscrit,
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(IDLE_SLEEP_SECS)).await;
                } else {
                    // More to do — loop again promptly (the per-file pauses
                    // already throttle the actual work).
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            } else {
                // Passe désactivée en cours de route : la campagne ouverte est
                // terminée, pas suspendue. La laisser ouverte la ferait
                // apparaître « en cours » jusqu'au prochain redémarrage.
                clore_campagne(
                    &registre,
                    &mut campagne,
                    &mut analysees,
                    &mut repos_deja_inscrit,
                );
                tokio::time::sleep(std::time::Duration::from_secs(IDLE_SLEEP_SECS)).await;
            }
        }
    });
}

/// Fermer la campagne ReplayGain en cours, ou inscrire un « rien à faire ».
///
/// Un seul endroit pour les deux transitions vers le repos, sinon le drapeau
/// `repos_deja_inscrit` finirait par diverger entre les deux branches et le
/// registre se remplirait de lignes vides.
fn clore_campagne(
    registre: &crate::db::task_run_repo::TaskRunRepo,
    campagne: &mut Option<crate::db::task_run_repo::Execution>,
    analysees: &mut i64,
    repos_deja_inscrit: &mut bool,
) {
    if let Some(e) = campagne.take() {
        let n = *analysees;
        e.terminer(
            crate::db::task_run_repo::Verdict::Succes,
            Some(n),
            Some(&format!("{n} elements analyses")),
        );
        *analysees = 0;
        *repos_deja_inscrit = true;
        return;
    }
    if !*repos_deja_inscrit {
        registre
            .ouvrir(crate::db::task_run_repo::TACHE_REPLAYGAIN)
            .rien_a_faire(Some("aucune piste ni album sans ReplayGain"));
        *repos_deja_inscrit = true;
    }
}

/// Analyse up to `TRACK_BATCH` local tracks that have no ReplayGain yet. Returns
/// how many were processed (0 ⇒ nothing left, caller idles).
pub async fn analyze_track_batch(backend: &Arc<dyn DbBackend>) -> usize {
    // Local tracks with a file on disk, not yet analysed (no `rg_analyzed`
    // sentinel) and without file-tag ReplayGain (`rg_track_gain`). The two
    // NOT EXISTS keep the sweep advancing and honour the file's own tags.
    let rows = match backend.query_many(
        "SELECT t.id, t.file_path, t.duration_ms, t.sample_rate, t.channels FROM tracks t \
         WHERE t.file_path IS NOT NULL AND t.file_path != '' \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_analyzed') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_track_gain') \
         LIMIT ?",
        &[&(TRACK_BATCH as i64) as &dyn ToSqlValue],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "replaygain_candidate_query_failed");
            return 0;
        }
    };
    if rows.is_empty() {
        debug!("replaygain_no_pending_tracks");
        return 0;
    }

    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let mut done = 0usize;
    for r in &rows {
        // Playback can start mid-batch; yield at once so a decode never
        // competes with the audio pipeline (#1310).
        if any_zone_playing(backend) {
            debug!("replaygain_yield_to_playback — zone playing, pausing sweep mid-batch");
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

        let est = estimated_analysis_bytes(
            r.get(2).and_then(|v| v.as_i64()),
            r.get(3).and_then(|v| v.as_i64()),
            r.get(4).and_then(|v| v.as_i64()),
        );
        if est > MAX_ANALYSIS_EST_BYTES {
            warn!(
                track_id,
                path = %path,
                estimated_mb = est / 1_048_576,
                "replaygain_skipped_oversized — full-decode analysis would risk \
                 OOM (#1109); will be analysed once streaming analysis lands"
            );
            let _ = repo.set(track_id, "rg_analyzed", &now_epoch_secs().to_string());
            let _ = repo.set(track_id, "rg_skipped_oversized", "1");
            continue;
        }

        let measured = tokio::time::timeout(
            std::time::Duration::from_secs(PER_TRACK_ANALYSIS_TIMEOUT_SECS),
            crate::audio::analyzer::measure_loudness_and_peak(&path),
        )
        .await;
        match measured {
            Ok(Some((lufs, peak))) => {
                let gain = track_gain_db(lufs);
                let _ = repo.set(track_id, "rg_track_gain", &format_gain(gain));
                let _ = repo.set(track_id, "rg_track_peak", &format_peak(peak));
            }
            Ok(None) => {
                // Undecodable / silent — still stamp so we don't retry forever.
                debug!(track_id, path = %path, "replaygain_measure_none");
            }
            Err(_elapsed) => {
                // The file blocked analysis (pathological decode / dormant NAS
                // mount) past the per-track bound. Stamp it analysed below so the
                // sweep ADVANCES instead of looping on it forever (#1155). The
                // orphaned blocking decode can't be cancelled, but the sentinel
                // keeps this file out of every future batch, so we hit it once.
                warn!(
                    track_id,
                    path = %path,
                    timeout_s = PER_TRACK_ANALYSIS_TIMEOUT_SECS,
                    "replaygain_measure_timeout — file stalled analysis; skipping so the sweep advances (#1155)"
                );
            }
        }
        // Sentinel = unix seconds, so an album pass can tell a track has been
        // handled even when it produced no gain.
        let _ = repo.set(track_id, "rg_analyzed", &now_epoch_secs().to_string());
        done += 1;

        tokio::time::sleep(std::time::Duration::from_millis(PER_FILE_PAUSE_MS)).await;
    }

    info!(analyzed = done, "replaygain_track_batch");
    done
}

/// Compute album ReplayGain for one album whose tracks are all analysed but that
/// still lacks album gain. Returns 1 if an album was processed, else 0.
///
/// Album gain uses the duration-weighted energy mean of the tracks' loudness
/// (recovered from each `rg_track_gain`), matching how ReplayGain 2.0 shares one
/// gain across an album to preserve inter-track dynamics; album peak is the max
/// track peak. Written to EVERY track of the album (ReplayGain album tags are
/// per-track).
pub fn analyze_album_batch(backend: &Arc<dyn DbBackend>) -> usize {
    // An album that has track gains but no album gain yet. One at a time keeps
    // it cheap (pure arithmetic, no decode) and interleaved with the track pass.
    let album_row = backend
        .query_one(
            "SELECT t.album_id FROM tracks t \
             JOIN track_metadata g ON g.track_id = t.id AND g.key = 'rg_track_gain' \
             WHERE t.album_id IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM track_metadata a \
                     WHERE a.track_id = t.id AND a.key = 'rg_album_gain') \
             LIMIT 1",
            &[],
        )
        .ok()
        .flatten();
    let album_id = match album_row.and_then(|r| r.first().and_then(|v| v.as_i64())) {
        Some(id) => id,
        None => return 0,
    };

    // All tracks of the album, with their gain, peak and duration.
    let rows = match backend.query_many(
        "SELECT t.id, t.duration_ms, \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_gain'), \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_peak') \
         FROM tracks t WHERE t.album_id = ?",
        &[&album_id as &dyn ToSqlValue],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, album_id, "replaygain_album_query_failed");
            return 0;
        }
    };

    let mut energy_sum = 0.0f64; // duration-weighted linear energy
    let mut dur_sum = 0.0f64;
    let mut peak_max = 0.0f64;
    let mut n = 0usize;
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let mut track_ids: Vec<i64> = Vec::new();

    for r in &rows {
        let tid = match r.first().and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => continue,
        };
        track_ids.push(tid);
        let dur = r.get(1).and_then(|v| v.as_i64()).unwrap_or(0).max(1) as f64;
        // gain string like "-6.50 dB" → lufs = REFERENCE - gain
        if let Some(gain) = r.get(2).and_then(|v| v.as_string()).and_then(parse_gain_db) {
            let lufs = REFERENCE_LUFS - gain;
            energy_sum += dur * 10f64.powf(lufs / 10.0);
            dur_sum += dur;
            n += 1;
        }
        if let Some(p) = r
            .get(3)
            .and_then(|v| v.as_string())
            .and_then(|s| s.parse::<f64>().ok())
        {
            peak_max = peak_max.max(p);
        }
    }

    if n == 0 || dur_sum <= 0.0 {
        return 0;
    }
    let album_lufs = 10.0 * (energy_sum / dur_sum).log10();
    let album_gain = track_gain_db(album_lufs);
    let gain_str = format_gain(album_gain);
    let peak_str = format_peak(peak_max);

    for tid in &track_ids {
        let _ = repo.set(*tid, "rg_album_gain", &gain_str);
        let _ = repo.set(*tid, "rg_album_peak", &peak_str);
    }
    info!(album_id, tracks = track_ids.len(), gain = %gain_str, "replaygain_album");
    1
}

/// Parse a ReplayGain gain string ("-6.50 dB", "+3.2", "-6.50dB") to dB.
fn parse_gain_db(s: String) -> Option<f64> {
    s.to_lowercase()
        .replace("db", "")
        .trim()
        .parse::<f64>()
        .ok()
}

// ---------------------------------------------------------------------------
// Applying the gain at playback
// ---------------------------------------------------------------------------
//
// Everything above MEASURES and STORES the gain. Nothing used to READ it back:
// a library could be fully analysed, every `rg_track_gain` in place, and not
// one decibel was ever applied. This is the consuming half.

/// Setting: `off` (default), `track` or `album`.
pub const MODE_KEY: &str = "replaygain_mode";
/// Setting: extra dB applied on top of the tag, e.g. `+3` for a quiet system.
pub const PREAMP_KEY: &str = "replaygain_preamp_db";
/// Setting: pull the gain back when the tagged peak says it would clip.
pub const PREVENT_CLIPPING_KEY: &str = "replaygain_prevent_clipping";

/// How the gain is chosen for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainMode {
    /// No gain is applied — the stream stays bit-identical to the source.
    Off,
    /// Per-track gain: every track plays at the same loudness.
    Track,
    /// Per-album gain: the relative dynamics between tracks of an album are
    /// preserved, which is what a classical or concept album needs.
    Album,
}

impl ReplayGainMode {
    pub fn from_setting(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "track" => Self::Track,
            "album" => Self::Album,
            _ => Self::Off,
        }
    }
}

/// The gain to apply to one track, in dB, plus the peak it was tagged with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackGain {
    pub gain_db: f64,
    pub peak: Option<f64>,
}

/// Resolved playback settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayGainSettings {
    pub mode: ReplayGainMode,
    pub preamp_db: f64,
    pub prevent_clipping: bool,
}

impl Default for ReplayGainSettings {
    fn default() -> Self {
        Self {
            mode: ReplayGainMode::Off,
            preamp_db: 0.0,
            prevent_clipping: true,
        }
    }
}

impl ReplayGainSettings {
    /// Read the three settings. Anything unreadable falls back to the default,
    /// which is `Off` — a broken setting must never silently alter the sound.
    pub fn load(backend: &Arc<dyn DbBackend>) -> Self {
        let settings = SettingsRepo::with_backend(backend.clone());
        let get = |k: &str| settings.get(k).ok().flatten();
        Self {
            mode: get(MODE_KEY)
                .map(|v| ReplayGainMode::from_setting(&v))
                .unwrap_or(ReplayGainMode::Off),
            preamp_db: get(PREAMP_KEY)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0)
                .clamp(-15.0, 15.0),
            prevent_clipping: get(PREVENT_CLIPPING_KEY)
                .map(|v| v != "false")
                .unwrap_or(true),
        }
    }
}

/// Read the gain stored for `track_id`, honouring the mode.
///
/// Album mode falls back to the track gain when the album values are missing —
/// half an album's worth of gain is still better than a jump in level.
pub fn stored_gain_for(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    mode: ReplayGainMode,
) -> Option<TrackGain> {
    stored_gain_detail(backend, track_id, mode).map(|(gain, _)| gain)
}

/// Comme [`stored_gain_for`], mais dit AUSSI quelle granularité a fourni la
/// valeur : en mode album, une piste sans tags d'album retombe sur le gain de
/// piste, et un affichage (chemin du signal) doit nommer ce qui s'applique
/// VRAIMENT, pas le réglage demandé.
pub fn stored_gain_detail(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    mode: ReplayGainMode,
) -> Option<(TrackGain, ReplayGainMode)> {
    if mode == ReplayGainMode::Off {
        return None;
    }
    let meta = TrackMetadataRepo::with_backend(backend.clone())
        .get_all(track_id)
        .ok()?;
    let pick = |gain_key: &str, peak_key: &str| -> Option<TrackGain> {
        let gain_db = meta.get(gain_key).cloned().and_then(parse_gain_db)?;
        let peak = meta
            .get(peak_key)
            .and_then(|p| p.trim().parse::<f64>().ok())
            .filter(|p| *p > 0.0);
        Some(TrackGain { gain_db, peak })
    };
    match mode {
        ReplayGainMode::Album => pick("rg_album_gain", "rg_album_peak")
            .map(|g| (g, ReplayGainMode::Album))
            .or_else(|| pick("rg_track_gain", "rg_track_peak").map(|g| (g, ReplayGainMode::Track))),
        _ => pick("rg_track_gain", "rg_track_peak").map(|g| (g, ReplayGainMode::Track)),
    }
}

/// The linear factor to multiply samples by: `1.0` means "leave the audio
/// alone".
///
/// Clipping prevention is not cosmetic. A loudness-war master tagged at
/// `peak = 1.0` with a positive gain would be pushed past full scale and
/// crunch on every peak — the listener would blame Tune, rightly. When the
/// tagged peak says the result would exceed full scale, the factor is pulled
/// back to exactly what fits.
pub fn gain_factor(gain: TrackGain, settings: ReplayGainSettings) -> f64 {
    if settings.mode == ReplayGainMode::Off {
        return 1.0;
    }
    let total_db = (gain.gain_db + settings.preamp_db).clamp(-30.0, 30.0);
    let mut factor = 10f64.powf(total_db / 20.0);
    if settings.prevent_clipping {
        if let Some(peak) = gain.peak {
            if peak > 0.0 && factor * peak > 1.0 {
                factor = 1.0 / peak;
            }
        }
    }
    // A factor below this is inaudible attenuation of a signal to nothing; a
    // factor above is a bug, not a preference.
    factor.clamp(0.001, 4.0)
}

/// Scale interleaved PCM in place by a linear factor.
///
/// For outputs that receive an encoded stream rather than rendering samples
/// themselves: the gain has to be baked in here or it never happens.
/// Saturating on the way out — a sample pushed past full scale wraps around
/// into a loud click if it is simply truncated.
pub fn apply_gain_pcm(pcm: &mut [u8], bit_depth: u16, factor: f64) {
    if pcm.is_empty() || (factor - 1.0).abs() < 1e-9 {
        return;
    }
    match bit_depth {
        16 => {
            for s in pcm.chunks_exact_mut(2) {
                let v = i16::from_le_bytes([s[0], s[1]]) as f64 * factor;
                let clamped = v.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                s.copy_from_slice(&clamped.to_le_bytes());
            }
        }
        24 => {
            const MAX: f64 = 8_388_607.0;
            const MIN: f64 = -8_388_608.0;
            for s in pcm.chunks_exact_mut(3) {
                // Sign-extend the 24-bit little-endian sample into an i32.
                let raw = ((s[2] as i32) << 24 | (s[1] as i32) << 16 | (s[0] as i32) << 8) >> 8;
                let v = (raw as f64 * factor).clamp(MIN, MAX) as i32;
                s[0] = (v & 0xFF) as u8;
                s[1] = ((v >> 8) & 0xFF) as u8;
                s[2] = ((v >> 16) & 0xFF) as u8;
            }
        }
        32 => {
            for s in pcm.chunks_exact_mut(4) {
                let raw = i32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                let v = (raw as f64 * factor).clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                s.copy_from_slice(&v.to_le_bytes());
            }
        }
        // 8-bit and anything exotic: leave the audio strictly alone rather
        // than guess at its encoding.
        _ => {}
    }
}

/// Convenience: the factor to apply for `track_id`, or `1.0` when ReplayGain is
/// off, unmeasured, or unreadable.
pub fn playback_factor(backend: &Arc<dyn DbBackend>, track_id: i64) -> f64 {
    let settings = ReplayGainSettings::load(backend);
    match stored_gain_for(backend, track_id, settings.mode) {
        Some(gain) => gain_factor(gain, settings),
        None => 1.0,
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod registre_tests {
    use std::sync::Arc;

    use crate::db::backend::DbBackend;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::db::task_run_repo::{Execution, TACHE_REPLAYGAIN, TaskRunRepo};

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Une campagne, c'est du premier lot trouvé jusqu'au retour au repos —
    /// PAS un lot. La boucle en enchaîne des centaines : une ligne par lot
    /// consommerait toute la rétention en quelques minutes sans jamais rien
    /// raconter d'autre que « ça avance ».
    #[test]
    fn une_campagne_couvre_toute_la_serie_de_lots_et_pas_un_lot() {
        let db = base();
        let registre = TaskRunRepo::with_backend(db.clone());
        let mut campagne: Option<Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos = false;

        // Trois lots successifs, comme la boucle les enchaîne.
        for lot in [12i64, 30, 8] {
            if campagne.is_none() {
                campagne = Some(registre.ouvrir(TACHE_REPLAYGAIN));
                analysees = 0;
            }
            analysees += lot;
            repos = false;
        }
        assert_eq!(
            registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap().len(),
            1,
            "trois lots, UNE ligne"
        );

        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos);

        let l = registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].outcome, "succes");
        assert_eq!(l[0].items, Some(50), "12 + 30 + 8, la campagne entière");
    }

    /// Le repos sans campagne s'inscrit UNE fois. C'est la réponse à « ça n'a
    /// rien fait » — sans elle, une bibliothèque déjà entièrement analysée
    /// serait indistinguable d'une passe jamais lancée.
    #[test]
    fn le_repos_s_inscrit_une_seule_fois_par_transition() {
        let db = base();
        let registre = TaskRunRepo::with_backend(db.clone());
        let mut campagne: Option<Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos = false;

        for _ in 0..5 {
            super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos);
        }

        let l = registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(
            l.len(),
            1,
            "cinq tours de boucle au repos, une seule ligne — sinon la \
             rétention serait remplie de lignes vides et l'historique utile \
             chassé"
        );
        assert_eq!(l[0].outcome, "rien_a_faire");
        assert_eq!(l[0].items, Some(0));
    }

    /// Fermer une campagne ne doit PAS écrire en plus une ligne « rien à
    /// faire » : la campagne est déjà la trace du passage. Deux lignes pour un
    /// seul retour au repos rendraient le registre illisible.
    #[test]
    fn fermer_une_campagne_n_ajoute_pas_une_ligne_de_repos() {
        let db = base();
        let registre = TaskRunRepo::with_backend(db.clone());
        let mut campagne: Option<Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos = false;

        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos);
        campagne = Some(registre.ouvrir(TACHE_REPLAYGAIN));
        analysees = 7;
        repos = false;
        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos);
        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos);

        let l = registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(l.len(), 2, "un repos, puis une campagne — et rien de plus");
        let verdicts: Vec<&str> = l.iter().map(|x| x.outcome.as_str()).collect();
        assert!(verdicts.contains(&"rien_a_faire"), "{verdicts:?}");
        assert!(verdicts.contains(&"succes"), "{verdicts:?}");
        let campagne_close = l.iter().find(|x| x.outcome == "succes").unwrap();
        assert_eq!(campagne_close.items, Some(7));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le garde-fou lecture partagé par les deux sweeps (#1310, #1515) : il lit
    /// `zones.last_play_state` et doit tomber en panne OUVERTE (false) si la
    /// table manque, pour ne jamais geler l'analyse sur un hoquet de base.
    #[test]
    fn any_zone_playing_reads_last_play_state_and_fails_open() {
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());

        // Table absente → erreur de requête → false (panne ouverte).
        assert!(!any_zone_playing(&backend));

        db.execute_batch(
            "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
             INSERT INTO zones (id, name, last_play_state)
                 VALUES (1, 'Salon', 'stopped'), (2, 'Bureau', 'paused');",
        )
        .unwrap();
        assert!(!any_zone_playing(&backend));

        db.execute_batch("UPDATE zones SET last_play_state = 'playing' WHERE id = 2;")
            .unwrap();
        assert!(any_zone_playing(&backend));

        // La garde doit NOMMER la zone qui bloque : sans ce nom, une analyse
        // figée par une zone restée à `playing` après un arrêt brutal est
        // indiagnosticable depuis les journaux (#1464, #1456, #1457).
        assert_eq!(playing_zone_name(&backend).as_deref(), Some("Bureau"));

        db.execute_batch("UPDATE zones SET last_play_state = 'stopped';")
            .unwrap();
        assert_eq!(playing_zone_name(&backend), None);
    }

    /// #1330 : la cadence DSD brute etait prise pour une cadence PCM, ce qui
    /// gonflait l'estimation d'un facteur 16 a 32 et ecartait TOUT fichier DSD
    /// de l'analyse.
    #[test]
    fn dsd_rate_is_converted_to_the_real_decode_rate() {
        // DSD256 (11,3 MHz) est decode a 352,8 kHz.
        assert_eq!(effective_decode_rate(11_289_600), 352_800);
        // DSD64 (2,8 MHz) est decode a 176,4 kHz.
        assert_eq!(effective_decode_rate(2_822_400), 176_400);
        // Une vraie cadence PCM n'est jamais touchee, y compris la plus haute.
        assert_eq!(effective_decode_rate(44_100), 44_100);
        assert_eq!(effective_decode_rate(768_000), 768_000);
    }

    /// Le cas exact de Cyrille : 6 minutes de DSD256 annoncees a ~100 Go.
    #[test]
    fn dsd256_estimate_is_no_longer_absurd() {
        let dur_ms = Some(372_000);
        let est = estimated_analysis_bytes(dur_ms, Some(11_289_600), Some(2));
        let gb = est as f64 / 1e9;
        assert!(
            (2.5..4.0).contains(&gb),
            "6 min de DSD256 devraient peser ~3 Go decodes, pas {gb} Go"
        );
    }

    /// Un DSD64 court entre desormais dans le budget, la ou il etait ecarte.
    #[test]
    fn short_dsd64_becomes_analysable() {
        let est = estimated_analysis_bytes(Some(240_000), Some(2_822_400), Some(2));
        assert!(
            est < MAX_ANALYSIS_EST_BYTES,
            "4 min de DSD64 devraient tenir dans le budget, estime a {est}"
        );
    }

    #[test]
    fn oversized_estimate_math() {
        // 30 min of 24/192 stereo ≈ 8 GB decoded+normalised: must exceed the budget.
        let est = estimated_analysis_bytes(Some(30 * 60 * 1000), Some(192_000), Some(2));
        assert!(est > MAX_ANALYSIS_EST_BYTES, "{est}");
        // A 5-minute CD track (~160 MB) sails under it.
        let cd = estimated_analysis_bytes(Some(5 * 60 * 1000), Some(44_100), Some(2));
        assert!(cd < MAX_ANALYSIS_EST_BYTES, "{cd}");
        // Unknown duration → 0 → never refused on missing data.
        assert_eq!(estimated_analysis_bytes(None, Some(192_000), Some(2)), 0);
        // Unknown rate/channels fall back to CD stereo, not to zero.
        assert!(estimated_analysis_bytes(Some(300_000), None, None) > 0);
    }

    #[test]
    fn gain_is_reference_minus_lufs() {
        // A track at -12 LUFS (louder than the -18 reference) attenuates by 6 dB.
        assert!((track_gain_db(-12.0) - (-6.0)).abs() < 1e-9);
        // A track at -23 LUFS (quieter) is boosted by +5 dB.
        assert!((track_gain_db(-23.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn format_roundtrip() {
        assert_eq!(format_gain(-6.5), "-6.50 dB");
        assert_eq!(format_peak(0.9885534), "0.988553");
        assert_eq!(parse_gain_db("-6.50 dB".into()), Some(-6.5));
        assert_eq!(parse_gain_db("3.20".into()), Some(3.2));
    }

    #[test]
    fn factor_is_one_when_off() {
        let g = TrackGain {
            gain_db: -6.0,
            peak: Some(0.9),
        };
        let s = ReplayGainSettings::default();
        assert_eq!(s.mode, ReplayGainMode::Off);
        assert_eq!(gain_factor(g, s), 1.0);
    }

    #[test]
    fn minus_six_db_halves_amplitude() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            ..Default::default()
        };
        let f = gain_factor(
            TrackGain {
                gain_db: -6.0206,
                peak: None,
            },
            s,
        );
        assert!((f - 0.5).abs() < 1e-4, "{f}");
    }

    #[test]
    fn preamp_adds_to_the_tag() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 6.0206,
            prevent_clipping: false,
        };
        // -6 dB tag + 6 dB pre-amp ⇒ unity.
        let f = gain_factor(
            TrackGain {
                gain_db: -6.0206,
                peak: None,
            },
            s,
        );
        assert!((f - 1.0).abs() < 1e-4, "{f}");
    }

    #[test]
    fn clipping_prevention_caps_at_the_peak() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
        };
        // +6 dB on a track already peaking at 0.95 would reach 1.9 — clipped.
        let f = gain_factor(
            TrackGain {
                gain_db: 6.0,
                peak: Some(0.95),
            },
            s,
        );
        assert!((f - 1.0 / 0.95).abs() < 1e-9, "{f}");
        assert!(f * 0.95 <= 1.0 + 1e-9);
        // Turned off, the same track is allowed to overshoot.
        let loose = ReplayGainSettings {
            prevent_clipping: false,
            ..s
        };
        assert!(
            gain_factor(
                TrackGain {
                    gain_db: 6.0,
                    peak: Some(0.95)
                },
                loose
            ) > 1.9
        );
    }

    #[test]
    fn clipping_prevention_never_boosts_a_quiet_track() {
        // A track peaking at 0.2 with a -3 dB tag must still be attenuated:
        // the peak cap is a ceiling, never a floor.
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
        };
        let f = gain_factor(
            TrackGain {
                gain_db: -3.0,
                peak: Some(0.2),
            },
            s,
        );
        assert!(f < 1.0, "{f}");
    }

    #[test]
    fn apply_gain_halves_16bit_samples() {
        let mut pcm = Vec::new();
        for v in [1000i16, -1000, 32767, -32768] {
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        apply_gain_pcm(&mut pcm, 16, 0.5);
        let got: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(got, vec![500, -500, 16383, -16384]);
    }

    #[test]
    fn apply_gain_saturates_instead_of_wrapping() {
        // Without the clamp this wraps to a large negative value — an audible
        // click on every peak, which is worse than the clipping it replaces.
        let mut pcm = 30000i16.to_le_bytes().to_vec();
        apply_gain_pcm(&mut pcm, 16, 2.0);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), i16::MAX);
    }

    #[test]
    fn apply_gain_handles_24bit_sign_extension() {
        // -1000 as a 24-bit little-endian sample.
        let v: i32 = -1000;
        let mut pcm = vec![
            (v & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
        ];
        apply_gain_pcm(&mut pcm, 24, 0.5);
        let raw = ((pcm[2] as i32) << 24 | (pcm[1] as i32) << 16 | (pcm[0] as i32) << 8) >> 8;
        assert_eq!(raw, -500);
    }

    #[test]
    fn apply_gain_of_one_is_a_no_op() {
        let original = vec![1u8, 2, 3, 4, 5, 6];
        let mut pcm = original.clone();
        apply_gain_pcm(&mut pcm, 16, 1.0);
        assert_eq!(pcm, original);
    }

    #[test]
    fn mode_parsing_defaults_to_off() {
        assert_eq!(ReplayGainMode::from_setting("track"), ReplayGainMode::Track);
        assert_eq!(ReplayGainMode::from_setting("ALBUM"), ReplayGainMode::Album);
        assert_eq!(ReplayGainMode::from_setting(""), ReplayGainMode::Off);
        assert_eq!(
            ReplayGainMode::from_setting("yes please"),
            ReplayGainMode::Off
        );
    }

    #[test]
    fn album_energy_mean_between_track_extremes() {
        // Duration-weighted energy mean of -12 and -18 LUFS must land between them.
        let e = (10f64.powf(-12.0 / 10.0) + 10f64.powf(-18.0 / 10.0)) / 2.0;
        let album_lufs = 10.0 * e.log10();
        assert!(album_lufs < -12.0 && album_lufs > -18.0);
    }
}
