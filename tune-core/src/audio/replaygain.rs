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

/// How long the sweep backs off after finding a zone actively playing. The
/// track pass fully decodes files — often over a network (SMB/NAS) mount — and
/// on a busy link that starves the same disk/network the player reads from,
/// stalling the audio pipeline (#1310, « la musique s'arrête au premier
/// morceau »). The pass yields entirely while anything plays, then rechecks.
const PLAYBACK_BACKOFF_SECS: u64 = 30;

/// The 12 B/sample estimate above, from the DB columns the scan filled.
/// Unknown rate/channels fall back to CD stereo; unknown duration returns 0
/// (no basis to refuse — the track is analysed as before).
fn estimated_analysis_bytes(
    duration_ms: Option<i64>,
    sample_rate: Option<i64>,
    channels: Option<i64>,
) -> u64 {
    let dur_s = match duration_ms {
        Some(ms) if ms > 0 => ms as u64 / 1000,
        _ => return 0,
    };
    let rate = sample_rate.filter(|&r| r > 0).unwrap_or(44_100) as u64;
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

/// Is the ReplayGain sweep enabled here?
///
/// Absent means ON: Bertrand wants calculated tags filled automatically, so a
/// library that never touched the setting still gets them. Only an explicit
/// "false" turns it off. Public so the settings route reports and flips exactly
/// what the sweep itself reads — one source of truth (forum #1310, where a
/// tester asked where to switch it off and the answer was "nowhere").
pub fn is_enabled(settings: &SettingsRepo) -> bool {
    settings
        .get(ENABLED_KEY)
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true)
}

/// Setting key backing [`is_enabled`].
pub const SETTING_KEY: &str = ENABLED_KEY;

fn enabled(settings: &SettingsRepo) -> bool {
    is_enabled(settings)
}

/// True if any zone is currently playing, per the persisted `last_play_state`
/// the orchestrator writes on every play/pause/stop. The ReplayGain track pass
/// must yield to playback (#1310): decoding whole files — often over a network
/// mount — otherwise saturates the same disk/network the player reads from and
/// stalls audio. Fails open (returns `false`) on a query error so a DB hiccup
/// can never freeze the sweep permanently.
fn any_zone_playing(backend: &Arc<dyn DbBackend>) -> bool {
    matches!(
        backend.query_one(
            "SELECT 1 FROM zones WHERE last_play_state = 'playing' LIMIT 1",
            &[],
        ),
        Ok(Some(_))
    )
}

/// Spawn the background ReplayGain analysis loop. Drains tracks that lack
/// ReplayGain, then idles; picks up any new tracks after later scans on its own.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    tokio::spawn(async move {
        // Let startup/scan settle before touching the disk hard.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            if enabled(&settings) {
                // Yield the decode-heavy track pass to playback (#1310). The
                // album pass is pure DB math (no file decode), so it keeps
                // making progress even while a zone plays.
                let playing = any_zone_playing(&backend);
                let did = if playing {
                    0
                } else {
                    analyze_track_batch(&backend).await
                };
                let albums = analyze_album_batch(&backend);
                if playing {
                    tokio::time::sleep(std::time::Duration::from_secs(PLAYBACK_BACKOFF_SECS)).await;
                } else if did == 0 && albums == 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(IDLE_SLEEP_SECS)).await;
                } else {
                    // More to do — loop again promptly (the per-file pauses
                    // already throttle the actual work).
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(IDLE_SLEEP_SECS)).await;
            }
        }
    });
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

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod enable_gate_tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn settings() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        // `settings` is created by the migrations, not by init_schema.
        crate::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::new(db)
    }

    /// Absent means ON here — the opposite of the acoustic sweep. A library
    /// that never touched the setting must keep getting its gain tags.
    #[test]
    fn replaygain_is_on_unless_explicitly_disabled() {
        let s = settings();
        assert!(is_enabled(&s), "absent setting must mean on");
        s.set(SETTING_KEY, "false").unwrap();
        assert!(!is_enabled(&s), "only an explicit false turns it off");
        s.set(SETTING_KEY, "true").unwrap();
        assert!(is_enabled(&s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn album_energy_mean_between_track_extremes() {
        // Duration-weighted energy mean of -12 and -18 LUFS must land between them.
        let e = (10f64.powf(-12.0 / 10.0) + 10f64.powf(-18.0 / 10.0)) / 2.0;
        let album_lufs = 10.0 * e.log10();
        assert!(album_lufs < -12.0 && album_lufs > -18.0);
    }
}
