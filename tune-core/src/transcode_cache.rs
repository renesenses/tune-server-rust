//! On-disk cache for pre-transcoded renditions.
//!
//! Network outputs (DLNA/UPnP) need a fully-encoded file with Content-Length +
//! Range support, so the orchestrator decodes and re-encodes the whole source
//! before playback can start — 30+ seconds for a hi-res track over a NAS.
//! Nothing reused that work: the temp file used a random UUID name and was
//! deleted when the stream session ended, so replaying a track (or a burst of
//! superseded taps on the same track) re-transcoded from scratch every time.
//!
//! This module gives those files a **deterministic** name derived from
//! everything that affects the encoded bytes, so an identical request finds the
//! finished file and serves it instantly. Cache files use the `tune-tcache-`
//! prefix, which `streamer::is_temp_transcode_file` does NOT match, so the
//! per-session and startup cleanups leave them alone — their lifetime is
//! governed here by [`evict`] (bounded total size, LRU).
//!
//! EQ is intentionally out of the key: a zone EQ curve changes the output, and
//! hashing the filter set here would be fragile. Callers must pass `None`-cache
//! (skip the cache) whenever a zone EQ is active.

use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};
use tracing::info;

/// Filename prefix for cached renditions. Deliberately distinct from the
/// `tune-transcode-` family so the streamer's cleanup never deletes these.
const CACHE_PREFIX: &str = "tune-tcache-";

/// A file modified within this window is never evicted — it may still be
/// streaming to a slow renderer. Matches the streamer's 1800s session GC.
const EVICT_MIN_AGE_SECS: u64 = 1800;

/// Default cache size cap (MiB) if `TUNE_TRANSCODE_CACHE_MAX_MB` is unset.
const DEFAULT_MAX_MB: u64 = 4096;

/// Minimum size (bytes) for a cache file to count as a completed transcode.
const MIN_VALID_BYTES: u64 = 1024;

/// Deterministic cache path for a transcoded rendition, or `None` if the
/// source metadata can't be read (→ caller falls back to a fresh transcode).
///
/// The key covers the source path, its mtime and size (so replacing or
/// re-tagging the file invalidates the entry), plus the target container and
/// the output sample rate / bit depth / channel count. It does NOT cover EQ —
/// see the module docs.
pub fn cache_path(
    source: &str,
    out_ext: &str,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
) -> Option<String> {
    let meta = std::fs::metadata(source).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    h.update(mtime.to_le_bytes());
    h.update(meta.len().to_le_bytes());
    h.update(out_ext.as_bytes());
    h.update(sample_rate.to_le_bytes());
    h.update(bit_depth.to_le_bytes());
    h.update(channels.to_le_bytes());
    let hex = format!("{:x}", h.finalize());
    let name = format!("{CACHE_PREFIX}{}.{out_ext}", &hex[..32]);
    Some(
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .to_string(),
    )
}

/// True when `path` holds a completed transcode (exists, non-trivial size).
pub fn is_hit(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() >= MIN_VALID_BYTES)
        .unwrap_or(false)
}

/// Mark a reused entry as recently used (bumps mtime) so LRU eviction keeps
/// hot files. Best-effort — a failure just means slightly less accurate LRU.
pub fn touch(path: &str) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Configured cache size cap in bytes (`TUNE_TRANSCODE_CACHE_MAX_MB`).
fn max_bytes() -> u64 {
    std::env::var("TUNE_TRANSCODE_CACHE_MAX_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_MB)
        .saturating_mul(1024 * 1024)
}

/// Evict least-recently-used cache files until the total is under the
/// configured cap (`TUNE_TRANSCODE_CACHE_MAX_MB`). Files touched within
/// `EVICT_MIN_AGE_SECS` are skipped (possibly in use). Pure filesystem I/O —
/// call from `spawn_blocking`, not the async executor.
pub fn evict() {
    evict_with_cap(max_bytes());
}

/// Eviction with an explicit byte cap (the testable core of [`evict`]).
fn evict_with_cap(cap: u64) {
    let dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = SystemTime::now();
    let mut files: Vec<(std::path::PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with(CACHE_PREFIX) {
            continue;
        }
        if let Ok(m) = entry.metadata() {
            if !m.is_file() {
                continue;
            }
            total += m.len();
            files.push((entry.path(), m.len(), m.modified().unwrap_or(now)));
        }
    }
    if total <= cap {
        return;
    }
    // Least-recently-used first.
    files.sort_by_key(|(_, _, mt)| *mt);
    let mut removed: u64 = 0;
    for (path, size, mtime) in files {
        if total <= cap {
            break;
        }
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age < Duration::from_secs(EVICT_MIN_AGE_SECS) {
            continue; // may still be serving
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed += size;
        }
    }
    if removed > 0 {
        info!(
            removed_bytes = removed,
            remaining_bytes = total,
            "transcode_cache_evicted"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_source(bytes: usize) -> String {
        // A unique real file so metadata() succeeds; content size varies the key.
        let p =
            std::env::temp_dir().join(format!("tcache-src-{}-{}.flac", std::process::id(), bytes));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn cache_path_is_deterministic_and_param_sensitive() {
        let src = tmp_source(100);
        let a = cache_path(&src, "flac", 44100, 16, 2).unwrap();
        let b = cache_path(&src, "flac", 44100, 16, 2).unwrap();
        assert_eq!(a, b, "same inputs → same path");
        assert!(a.contains("tune-tcache-"));
        assert!(a.ends_with(".flac"));

        // Any output-affecting param changes the path.
        assert_ne!(a, cache_path(&src, "wav", 44100, 16, 2).unwrap());
        assert_ne!(a, cache_path(&src, "flac", 48000, 16, 2).unwrap());
        assert_ne!(a, cache_path(&src, "flac", 44100, 24, 2).unwrap());
        assert_ne!(a, cache_path(&src, "flac", 44100, 16, 1).unwrap());
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn cache_path_none_for_missing_source() {
        assert!(cache_path("/no/such/file.flac", "flac", 44100, 16, 2).is_none());
    }

    #[test]
    fn is_hit_requires_completed_file() {
        let p = std::env::temp_dir().join(format!("tune-tcache-hit-{}.flac", std::process::id()));
        let ps = p.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&p);
        assert!(!is_hit(&ps), "missing → miss");
        std::fs::write(&p, vec![0u8; 10]).unwrap();
        assert!(!is_hit(&ps), "tiny file → miss");
        std::fs::write(&p, vec![0u8; 2048]).unwrap();
        assert!(is_hit(&ps), "completed file → hit");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn evict_never_removes_recent_files() {
        // A freshly written cache file is younger than EVICT_MIN_AGE_SECS, so
        // even with a 0-byte cap it must survive (it may be streaming).
        let p =
            std::env::temp_dir().join(format!("tune-tcache-recent-{}.flac", std::process::id()));
        std::fs::write(&p, vec![0u8; 4096]).unwrap();
        // Cap of 0 forces eviction pressure; the file is younger than
        // EVICT_MIN_AGE_SECS so it must still survive.
        evict_with_cap(0);
        assert!(p.exists(), "recent file must not be evicted");
        let _ = std::fs::remove_file(&p);
    }
}
