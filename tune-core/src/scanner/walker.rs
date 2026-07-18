use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rayon::prelude::*;
use tracing::{info, warn};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use super::hasher::compute_audio_hash;
use crate::metadata::{TrackMetadata, tagless_fallback_no_props, try_read_metadata};

/// Maximum time allowed for reading metadata + computing hash for a single file.
/// Files on NAS over a flaky network can hang indefinitely; this prevents the
/// entire scan from stalling on a single corrupt or unreachable file.
// Large Hi-Res FLAC (24/96, big embedded art) on slow/network storage can take
// well over 10s just to read tags via lofty — 10s wrongly skipped them entirely
// (Progman: files dropped from the library). Give more headroom, and on timeout
// fall back to filename metadata instead of losing the file.
const FILE_TIMEOUT: Duration = Duration::from_secs(30);

// Slow network storage (a NAS, or an SSD hanging off a UPnP streamer accessed
// over the LAN — Pierre M's NAS, Philippe Landes' Hifi Rose RS130) regularly
// exceeds FILE_TIMEOUT on the *first* tag read but succeeds with more headroom.
// Falling straight back to filename-only metadata left those tracks with
// duration = 0, which breaks gapless end-detection (the track is cut short or
// the queue stops advancing). Retry once with a much larger budget before
// giving up, so the real duration/tags are recovered.
const RETRY_FILE_TIMEOUT: Duration = Duration::from_secs(90);

// The audio hash (duplicate detection) reads the whole file, separately from
// the tags. Give it its own, larger budget so big Hi-Res files over a NAS still
// get hashed — but it's best-effort: on timeout the track keeps its real tags
// and only the hash is skipped (Progman: 23-min FLAC 24/88.2 exceeded 30s).
const HASH_TIMEOUT: Duration = Duration::from_secs(120);

// Per-file metadata reads are I/O-bound: each rayon task blocks on the tag read
// (lofty), frequently on high-latency network storage. The default rayon pool
// has only ~CPU-core-count threads, so effective concurrency — and throughput —
// was capped at the core count, which made a full scan ~10x slower than a
// tag-only indexer like MinimServer (Pierre M; Philippe Landes: 12h for 20200
// DSD tracks). Read metadata on a dedicated, higher-concurrency pool so many
// more per-file latencies overlap. Mirrors the 32-thread stat pool already used
// for the mtime pre-check (#619).
const SCAN_IO_CONCURRENCY: usize = 32;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff", "aif", "wv", "wma", "dsf", "dff", "dst",
    "alac", "ape", "iso",
];

const SKIP_DIRS: &[&str] = &[
    "duplicates",
    ".tune",
    ".Spotlight-V100",
    ".Trashes",
    "@eaDir",
    "#recycle",
    ".DS_Store",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Normalize a directory path for cross-platform compatibility.
///
/// On Windows, paths may use either `/` or `\` as separators. Users may also
/// add trailing slashes. This function:
/// - Converts forward slashes to the OS-native separator
/// - Strips trailing separators (except for root paths like `C:\` or `/`)
/// - Preserves UNC paths (`\\server\share`)
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // On Windows, normalize forward slashes to backslashes so that
    // std::path operations and WalkDir work with a consistent separator.
    #[cfg(target_os = "windows")]
    let normalized = trimmed.replace('/', "\\");
    #[cfg(not(target_os = "windows"))]
    let normalized = trimmed.to_string();

    // Strip trailing separator, but keep root paths intact (e.g. `C:\` or `/`)
    let result = normalized.trim_end_matches(['/', '\\']);
    if result.is_empty() {
        // Was just "/" or "\"
        return normalized.chars().next().unwrap().to_string();
    }

    // Keep the trailing separator for Windows drive roots like "C:"
    #[cfg(target_os = "windows")]
    if result.len() == 2 && result.as_bytes()[1] == b':' {
        return format!("{result}\\");
    }

    result.to_string()
}

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: String,
    pub metadata: Option<TrackMetadata>,
    pub audio_hash: Option<String>,
    pub file_size: u64,
    pub mtime: u64,
}

#[derive(Debug, Default)]
pub struct ScanStats {
    pub total_files: usize,
    pub metadata_ok: usize,
    pub metadata_failed: usize,
    pub metadata_timeout: usize,
    pub hash_ok: usize,
    pub failed_paths: Vec<String>,
}

/// Read metadata (and optionally compute hash) for a single file, with a
/// [`FILE_TIMEOUT`] guard.  If the underlying I/O does not complete in time
/// the file is skipped and `Err("timeout")` is returned.
///
/// We spawn a real OS thread because the metadata/hash reads are blocking I/O
/// that can hang on NAS mounts — `rayon` tasks must not block indefinitely.
/// Read tags (and optionally the audio hash), retrying once with a larger tag
/// budget on timeout. On slow network storage the first `FILE_TIMEOUT` read
/// often times out but a second, longer read succeeds — recovering the real
/// duration instead of leaving the track at duration 0. (Pierre M's NAS,
/// Philippe Landes' RS130 SSD)
fn read_file_with_retry(
    path: &PathBuf,
    with_hash: bool,
) -> Result<(Option<TrackMetadata>, Option<String>), String> {
    match read_file_with_timeout(path, with_hash, FILE_TIMEOUT) {
        Err(ref reason) if reason == "timeout" => {
            read_file_with_timeout(path, with_hash, RETRY_FILE_TIMEOUT)
        }
        other => other,
    }
}

fn read_file_with_timeout(
    path: &PathBuf,
    with_hash: bool,
    tag_timeout: Duration,
) -> Result<(Option<TrackMetadata>, Option<String>), String> {
    // Phase 1 — read the tags. This is fast even on a NAS (only the header /
    // tag blocks are read), so `tag_timeout` is plenty. A timeout here means the
    // tags are genuinely unreadable → caller falls back to filename metadata.
    let meta_path = path.clone();
    let (mtx, mrx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = mtx.send(try_read_metadata(&meta_path));
    });
    let metadata = match mrx.recv_timeout(tag_timeout) {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("timeout".to_string()),
    };

    if !with_hash {
        return Ok((Some(metadata), None));
    }

    // Phase 2 — compute the audio hash (used only for duplicate detection). This
    // reads the WHOLE file, which on very large Hi-Res files over a NAS can far
    // exceed the tag-read budget (Progman: a 23-min FLAC 24/88.2 ≈ 1 GB). Make
    // it best-effort: if it doesn't finish in HASH_TIMEOUT, keep the real tags
    // and just skip the hash (audio_hash = None) instead of dropping the track
    // to filename-only metadata. Dedup is degraded for that one file, nothing
    // more.
    let hash_path = path.clone();
    let (htx, hrx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = htx.send(compute_audio_hash(&hash_path));
    });
    let hash = hrx.recv_timeout(HASH_TIMEOUT).unwrap_or(None);
    Ok((Some(metadata), hash))
}

pub struct ListAudioResult {
    pub files: Vec<PathBuf>,
    pub missing_dirs: Vec<String>,
}

impl ListAudioResult {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.missing_dirs.is_empty()
    }
}

pub fn list_audio_files(dirs: &[String]) -> ListAudioResult {
    let extensions: HashSet<&str> = SUPPORTED_EXTENSIONS.iter().copied().collect();
    let skip_set: HashSet<&str> = SKIP_DIRS.iter().copied().collect();

    let mut files = Vec::new();
    let mut missing_dirs = Vec::new();
    for dir in dirs {
        let normalized = normalize_path(dir);
        let dir_path = std::path::Path::new(&normalized);

        // Probe with read_dir instead of a bare exists(): on Windows a NAS path
        // fails for several distinct reasons that exists() collapses to `false`
        // (silent skip → "scan finds nothing", Alain Bonnel). read_dir surfaces
        // the actual io::Error kind so the user learns WHY: NotFound = bad UNC /
        // NAS unmounted, PermissionDenied = no SMB credentials for this session,
        // and — the common Windows case — a mapped drive (Z:\) is invisible to
        // an elevated / service token even though it works in Explorer.
        if let Err(e) = std::fs::read_dir(dir_path) {
            warn!(
                dir = %normalized,
                original = %dir,
                error = %e,
                kind = ?e.kind(),
                "scan_dir_unreadable — cannot open directory (unreachable NAS, mapped drive not visible to this session, or permission denied), skipping"
            );
            missing_dirs.push(normalized);
            continue;
        }
        if !dir_path.is_dir() {
            warn!(
                dir = %normalized,
                "scan_dir_not_a_directory — path is not a directory, skipping"
            );
            continue;
        }

        let mut dir_file_count = 0usize;
        let mut dir_error_count = 0usize;

        let walker = WalkDir::new(&normalized)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    !skip_set.contains(name.as_ref())
                } else {
                    true
                }
            });

        for entry in walker {
            match entry {
                Ok(entry) => {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    // Skip macOS AppleDouble sidecar files (._foo.flac): they carry
                    // the audio extension but are tiny resource-fork metadata, not
                    // real tracks, and were being indexed as bogus duplicates (Elie).
                    if entry.file_name().to_string_lossy().starts_with("._") {
                        continue;
                    }
                    let path = entry.path();
                    if let Some(ext) = path.extension().and_then(|e| e.to_str())
                        && extensions.contains(ext.to_lowercase().as_str())
                    {
                        // ISO SACD: extract DSF tracks instead of adding the ISO directly
                        if ext.eq_ignore_ascii_case("iso")
                            && crate::audio::iso_sacd::is_sacd_iso(path)
                        {
                            match crate::audio::iso_sacd::extract_iso_to_dsf(path) {
                                Ok(dsf_files) => {
                                    dir_file_count += dsf_files.len();
                                    files.extend(dsf_files);
                                }
                                Err(e) => {
                                    warn!(path = %path.display(), error = %e, "sacd_iso_extract_failed");
                                    dir_error_count += 1;
                                }
                            }
                        } else {
                            files.push(path.to_path_buf());
                            dir_file_count += 1;
                        }
                    }
                }
                Err(err) => {
                    dir_error_count += 1;
                    if dir_error_count <= 5 {
                        warn!(
                            dir = %normalized,
                            error = %err,
                            "scan_walk_error — error while walking directory"
                        );
                    }
                }
            }
        }

        if dir_error_count > 5 {
            warn!(
                dir = %normalized,
                total_errors = dir_error_count,
                "scan_walk_errors_truncated — additional walk errors suppressed"
            );
        }

        info!(
            dir = %normalized,
            files = dir_file_count,
            errors = dir_error_count,
            "scan_dir_complete"
        );
    }

    info!(
        count = files.len(),
        dirs = dirs.len(),
        missing = missing_dirs.len(),
        "audio_files_listed"
    );
    ListAudioResult {
        files,
        missing_dirs,
    }
}

pub fn scan_files_parallel(
    files: &[PathBuf],
    with_hash: bool,
    progress_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (Vec<ScannedFile>, ScanStats) {
    let counter = AtomicUsize::new(0);
    let timeout_counter = AtomicUsize::new(0);
    let total = files.len();
    let failed_files: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let results: Vec<ScannedFile> = files
        .par_iter()
        .map(|path| {
            let idx = counter.fetch_add(1, Ordering::Relaxed);
            if let Some(ref cb) = progress_callback
                && idx.is_multiple_of(100)
            {
                cb(idx, total);
            }

            // NFC-normalize the path string: macOS HFS+/APFS stores filenames
            // in NFD (decomposed Unicode, e.g. "è" = "e" + combining accent).
            // Without NFC normalization, metadata readers and DB lookups can
            // fail on paths containing accented characters.
            let path_str: String = path.to_string_lossy().nfc().collect();

            let file_meta = path.metadata().ok();
            let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = file_meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let (metadata, audio_hash) = match read_file_with_retry(path, with_hash) {
                Ok((meta, hash)) => {
                    if meta.is_none() {
                        warn!(
                            path = %path_str,
                            "scan_file_no_metadata — metadata reader returned None"
                        );
                    }
                    (meta, hash)
                }
                Err(ref reason) if reason == "timeout" => {
                    // Don't drop the file — index it with filename-based metadata
                    // so it still appears in the library. audio_hash stays None so
                    // the next scan re-reads full tags once storage is responsive.
                    warn!(
                        path = %path_str,
                        timeout_secs = FILE_TIMEOUT.as_secs(),
                        "scan_file_timeout — tag read timed out, indexing with filename metadata"
                    );
                    timeout_counter.fetch_add(1, Ordering::Relaxed);
                    (Some(tagless_fallback_no_props(path)), None)
                }
                Err(ref err) => {
                    warn!(
                        path = %path_str,
                        error = %err,
                        "scan_file_metadata_failed — could not read metadata"
                    );
                    failed_files
                        .lock()
                        .unwrap()
                        .push((path_str.clone(), err.clone()));
                    (None, None)
                }
            };

            ScannedFile {
                path: path_str,
                metadata,
                audio_hash,
                file_size,
                mtime,
            }
        })
        .collect();

    let timed_out = timeout_counter.load(Ordering::Relaxed);
    let failed = failed_files.lock().unwrap();
    let failed_paths: Vec<String> = failed
        .iter()
        .map(|(p, e)| format!("{} ({})", p, e))
        .collect();
    let stats = ScanStats {
        total_files: results.len(),
        metadata_ok: results.iter().filter(|f| f.metadata.is_some()).count(),
        metadata_failed: results.iter().filter(|f| f.metadata.is_none()).count(),
        metadata_timeout: timed_out,
        hash_ok: results.iter().filter(|f| f.audio_hash.is_some()).count(),
        failed_paths,
    };
    if !failed.is_empty() {
        let listing: Vec<String> = failed
            .iter()
            .map(|(p, e)| format!("  {} ({})", p, e))
            .collect();
        warn!(
            count = failed.len(),
            "scan_metadata_failed_summary\n{}",
            listing.join("\n")
        );
    }
    drop(failed);

    if timed_out > 0 {
        warn!(
            count = timed_out,
            timeout_secs = FILE_TIMEOUT.as_secs(),
            "scan_timeout_summary — files skipped due to timeout"
        );
    }

    info!(
        total = stats.total_files,
        metadata_ok = stats.metadata_ok,
        metadata_failed = stats.metadata_failed,
        metadata_timeout = stats.metadata_timeout,
        "parallel_scan_complete"
    );

    (results, stats)
}

/// Default batch size for chunked scanning.
/// Balances memory usage vs. rayon thread-pool efficiency.
pub const SCAN_BATCH_SIZE: usize = 500;

/// Scan files in batches, calling `on_batch` after each chunk is parsed.
///
/// This enables **progressive availability**: each batch can be committed to
/// the database independently, so tracks are queryable as soon as each batch
/// finishes — not only after the entire scan completes.
///
/// The callback receives `(batch: Vec<ScannedFile>, batch_index: usize, total_files: usize)`.
/// It runs on a rayon worker thread, so the caller must ensure any shared
/// state (DB handle, caches) is `Send + Sync`.
///
/// Returns aggregate `ScanStats` over all batches.
pub fn scan_files_batched(
    files: &[PathBuf],
    with_hash: bool,
    batch_size: usize,
    mut on_batch: impl FnMut(Vec<ScannedFile>, usize, usize),
) -> ScanStats {
    let total = files.len();
    let batch_sz = if batch_size == 0 {
        SCAN_BATCH_SIZE
    } else {
        batch_size
    };
    let mut aggregate = ScanStats::default();
    aggregate.total_files = total;

    // Dedicated high-concurrency pool for the I/O-bound tag reads (see
    // SCAN_IO_CONCURRENCY). Built once and reused across batches. If the pool
    // fails to build, fall back to the default rayon pool.
    let io_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(SCAN_IO_CONCURRENCY)
        .thread_name(|i| format!("scan-io-{i}"))
        .build()
        .ok();

    for (batch_idx, chunk) in files.chunks(batch_sz).enumerate() {
        // Parse metadata in parallel within this chunk
        let failed_files: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let batch_timeout_counter = AtomicUsize::new(0);

        let read_batch = || {
            chunk
                .par_iter()
                .map(|path| {
                    // NFC-normalize: see comment in scan_files_parallel
                    let path_str: String = path.to_string_lossy().nfc().collect();

                    let file_meta = path.metadata().ok();
                    let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let mtime = file_meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    let (metadata, audio_hash) = match read_file_with_retry(path, with_hash) {
                        Ok((meta, hash)) => (meta, hash),
                        Err(ref reason) if reason == "timeout" => {
                            warn!(
                                path = %path_str,
                                timeout_secs = FILE_TIMEOUT.as_secs(),
                                "scan_file_timeout — file skipped (metadata read exceeded timeout)"
                            );
                            batch_timeout_counter.fetch_add(1, Ordering::Relaxed);
                            (None, None)
                        }
                        Err(err) => {
                            warn!(
                                path = %path_str,
                                error = %err,
                                "scan_file_failed"
                            );
                            failed_files.lock().unwrap().push((path_str.clone(), err));
                            (None, None)
                        }
                    };

                    ScannedFile {
                        path: path_str,
                        metadata,
                        audio_hash,
                        file_size,
                        mtime,
                    }
                })
                .collect()
        };
        // Run the I/O-bound reads on the dedicated high-concurrency pool so many
        // per-file latencies overlap; fall back to the default pool if the
        // dedicated one couldn't be built.
        let batch: Vec<ScannedFile> = match &io_pool {
            Some(pool) => pool.install(read_batch),
            None => read_batch(),
        };

        let batch_timeouts = batch_timeout_counter.load(Ordering::Relaxed);

        // Update aggregate stats
        aggregate.metadata_ok += batch.iter().filter(|f| f.metadata.is_some()).count();
        aggregate.metadata_failed += batch.iter().filter(|f| f.metadata.is_none()).count();
        aggregate.metadata_timeout += batch_timeouts;
        aggregate.hash_ok += batch.iter().filter(|f| f.audio_hash.is_some()).count();

        let failed = failed_files.lock().unwrap();
        if !failed.is_empty() {
            for (p, e) in failed.iter() {
                aggregate.failed_paths.push(format!("{} ({})", p, e));
            }
            let listing: Vec<String> = failed
                .iter()
                .take(10)
                .map(|(p, e)| format!("  {} ({})", p, e))
                .collect();
            warn!(
                count = failed.len(),
                batch = batch_idx,
                "scan_batch_failures\n{}",
                listing.join("\n")
            );
        }
        drop(failed);

        if batch_timeouts > 0 {
            warn!(
                count = batch_timeouts,
                batch = batch_idx,
                timeout_secs = FILE_TIMEOUT.as_secs(),
                "scan_batch_timeouts — files skipped due to timeout"
            );
        }

        info!(
            batch = batch_idx,
            batch_size = batch.len(),
            scanned = (batch_idx + 1) * batch_sz,
            total,
            "scan_batch_complete"
        );

        on_batch(batch, batch_idx, total);
    }

    if aggregate.metadata_timeout > 0 {
        warn!(
            count = aggregate.metadata_timeout,
            timeout_secs = FILE_TIMEOUT.as_secs(),
            "scan_timeout_summary — files skipped due to timeout"
        );
    }

    info!(
        total = aggregate.total_files,
        metadata_ok = aggregate.metadata_ok,
        metadata_failed = aggregate.metadata_failed,
        metadata_timeout = aggregate.metadata_timeout,
        "batched_scan_complete"
    );

    aggregate
}

pub fn scan_directories(
    dirs: &[String],
    with_hash: bool,
    progress_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (Vec<ScannedFile>, ScanStats) {
    let result = list_audio_files(dirs);
    scan_files_parallel(&result.files, with_hash, progress_callback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions_list() {
        assert!(SUPPORTED_EXTENSIONS.contains(&"flac"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"mp3"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"dsf"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"ape"));
        assert!(!SUPPORTED_EXTENSIONS.contains(&"txt"));
    }

    #[test]
    fn skip_dirs_list() {
        assert!(SKIP_DIRS.contains(&".DS_Store"));
        assert!(SKIP_DIRS.contains(&"@eaDir"));
        assert!(SKIP_DIRS.contains(&"$RECYCLE.BIN"));
    }

    #[test]
    fn list_nonexistent_dir() {
        let result = list_audio_files(&["/tmp/nonexistent_tune_test_dir".into()]);
        // No audio files found; the missing directory is tracked separately.
        assert!(result.files.is_empty());
        assert_eq!(result.missing_dirs.len(), 1);
    }

    #[test]
    fn scan_empty() {
        let (results, stats) = scan_directories(&[], false, None);
        assert!(results.is_empty());
        assert_eq!(stats.total_files, 0);
    }

    #[test]
    fn normalize_path_trailing_slash() {
        assert_eq!(normalize_path("/music/"), "/music");
        assert_eq!(normalize_path("/music"), "/music");
    }

    #[test]
    fn normalize_path_empty() {
        assert_eq!(normalize_path(""), "");
        assert_eq!(normalize_path("  "), "");
    }

    #[test]
    fn normalize_path_root() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_path_whitespace() {
        assert_eq!(normalize_path("  /music/flac  "), "/music/flac");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_backslash() {
        assert_eq!(
            normalize_path("C:\\Users\\Bob\\Music"),
            "C:\\Users\\Bob\\Music"
        );
        assert_eq!(
            normalize_path("C:\\Users\\Bob\\Music\\"),
            "C:\\Users\\Bob\\Music"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_forward_slash() {
        assert_eq!(
            normalize_path("C:/Users/Bob/Music"),
            "C:\\Users\\Bob\\Music"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_drive_root() {
        assert_eq!(normalize_path("C:\\"), "C:\\");
        assert_eq!(normalize_path("D:\\"), "D:\\");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_unc() {
        assert_eq!(normalize_path("\\\\NAS\\Musique"), "\\\\NAS\\Musique");
        assert_eq!(normalize_path("//NAS/Musique"), "\\\\NAS\\Musique");
    }
}
