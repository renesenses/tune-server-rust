use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rayon::prelude::*;
use tracing::{debug, info, warn};
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

// The audio hash (duplicate detection) does NOT read the whole file: it MD5s
// a single 64 KB sample at the 25% offset (scanner/hasher.rs). Its real cost
// is open + seek + one read — cheap even for huge files. The generous budget
// exists because on a stalled NAS those three syscalls can hang like any
// other I/O, and hashing is best-effort: on timeout the track keeps its real
// tags and only the hash is skipped (Progman: stalled mount, not file size).
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

/// Resolve the scan I/O concurrency, honouring an optional `TUNE_SCAN_IO_CONCURRENCY`
/// override. The fixed default (32) is a good fit for a fast SSD NAS, but the
/// sweet spot is storage-specific: a weak 2-bay HDD NAS (Synology DS218Play,
/// forum #1194) can be *slowed* by 32 concurrent reads scattered across the
/// platters (each file needs a seek for the tags at the start + a seek to 25%
/// for the dup-detection hash), while a high-latency share benefits from even
/// more. Rather than guess, let the operator tune it against their own NAS —
/// something we cannot benchmark centrally. Empty/invalid/zero → the default;
/// clamped to 1..=256 so a typo can't spawn a pathological number of OS threads.
pub fn scan_io_concurrency() -> usize {
    if let Some(n) = std::env::var("TUNE_SCAN_IO_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .map(|n| n.clamp(1, 256))
    {
        return n;
    }
    concurrence_pour_disque(disque_rotatif())
}

/// Concurrence adaptée au type de disque, une fois le réglage manuel écarté.
///
/// Fonction PURE, pour qu'elle soit testable sans `/sys` ni disque : la sonde
/// système est séparée dans `disque_rotatif`.
///
/// Sur plateaux, 32 lectures concurrentes ne parallélisent rien — elles font
/// osciller une tête unique entre 32 endroits. Chaque fichier demande deux
/// déplacements (les tags au début, le hachage de déduplication à 25 %), donc
/// 64 déplacements en vol sur un seul actionneur. Mesuré chez Yacine, 49 488
/// fichiers sur un disque USB : **5,8 fichiers/s**, contre 44 000 en une
/// trentaine de secondes sur SSD dans notre propre README (#1948).
///
/// `None` — type inconnu, ou pas Linux — garde la valeur d'origine : on ne
/// dégrade pas un SSD par prudence mal placée.
pub(crate) fn concurrence_pour_disque(rotatif: Option<bool>) -> usize {
    match rotatif {
        Some(true) => 4,
        _ => SCAN_IO_CONCURRENCY,
    }
}

/// Le stockage des dossiers de musique est-il sur plateaux ?
///
/// Lit `/sys/dev/block/<major>:<minor>/queue/rotational` — `1` = plateaux.
/// Pour une partition, `queue/` vit sur le disque parent, d'où le repli sur
/// `../queue/rotational`.
///
/// Rend `None` hors Linux, ou si quoi que ce soit dans la chaîne échoue : ce
/// n'est qu'une heuristique de performance, elle ne doit jamais empêcher un
/// scan. Lue une seule fois par processus — le pool n'est construit qu'une
/// fois, et un disque ne change pas de nature en cours de route.
pub(crate) fn disque_rotatif() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let racine = std::env::var("TUNE_MUSIC_DIRS")
            .ok()
            .and_then(|v| {
                serde_json::from_str::<Vec<String>>(&v)
                    .ok()
                    .and_then(|d| d.into_iter().next())
            })
            .unwrap_or_else(|| "/".to_string());
        let dev = std::fs::metadata(&racine).ok()?.dev();
        let (majeur, mineur) = (unsafe { libc::major(dev) }, unsafe { libc::minor(dev) });
        let base = format!("/sys/dev/block/{majeur}:{mineur}");
        for chemin in [
            format!("{base}/queue/rotational"),
            format!("{base}/../queue/rotational"),
        ] {
            if let Ok(v) = std::fs::read_to_string(&chemin) {
                return match v.trim() {
                    "1" => Some(true),
                    "0" => Some(false),
                    _ => None,
                };
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Lower CPU and I/O priority of the calling thread (Linux only, no-op
/// elsewhere). Applied to the dedicated scan pool threads only — never to
/// shared tokio pools — so a full scan stays in the background instead of
/// starving the UI and playback on small machines (USB-key appliance,
/// laptop CPUs: Stéphane, Tune OS). I/O class is best-effort level 7, not
/// IDLE: IDLE can starve the scan completely under continuous playback.
fn lower_scan_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        // Linux semantics: PRIO_PROCESS with a tid targets that thread only.
        // (`as _` : glibc type __priority_which_t vs c_int selon les libc.)
        libc::setpriority(libc::PRIO_PROCESS as _, tid as libc::id_t, 10);
        const IOPRIO_CLASS_BE: libc::c_long = 2;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        let ioprio = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid, ioprio);
    }
}

/// The dedicated I/O thread pool for tag reads, built once for the whole
/// process. Returns `None` (→ caller falls back to the default rayon pool) if
/// the pool couldn't be built. Reusing it avoids spawning and tearing down
/// SCAN_IO_CONCURRENCY OS threads on every scan pass.
fn scan_io_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(scan_io_concurrency())
            .thread_name(|i| format!("scan-io-{i}"))
            .start_handler(|_| lower_scan_thread_priority())
            .build()
            .ok()
    })
    .as_ref()
}

/// Audio extensions recognised by the scanner. Shared with the file watcher
/// (which excludes "iso": ISO SACD needs the extraction step that only the
/// full directory walk performs).
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff", "aif", "wv", "wma", "dsf", "dff", "dst",
    "alac", "ape", "iso",
];

/// Audio (or audio-describing) extensions Tune does NOT read, listed so the scan
/// report can say WHY files were left out instead of letting them vanish.
///
/// Deliberately a fixed list rather than "anything not in SUPPORTED_EXTENSIONS":
/// a music library is full of `.jpg`, `.nfo`, `.m3u`, `.log` and `.accurip`, and
/// counting those would drown the one line the user can act on.
///
/// `cue` earns its place even though it is not audio: a `.cue` next to a single
/// large file is precisely how an album gets stored as one track, so its
/// presence explains a missing album better than anything else.
pub const KNOWN_UNREAD_AUDIO: &[&str] = &[
    "mpc", "mp+", "mpp", // Musepack (Rhorn, #1763)
    "cue", // feuille de découpe, jamais interprétée
    "tta", "shn", "ofr", "ofs", // sans perte, formats de niche
    "m4b", "m4p", // livres audio, achats protégés
    "dts", "ac3", "eac3", "mka", // conteneurs plutôt vidéo/multicanal
    "aac", // AAC brut, hors conteneur m4a
    "ra", "rm", "amr", "spx",
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

/// Best-effort DSD (DSF/DFF) duration from the file header, bounded so a stalled
/// mount can't re-hang the scan. Used only in the timeout fallback: even when
/// lofty's full tag read timed out (big embedded art over slow storage), the
/// ~92-byte DSD header usually still reads fine, so the track gets a real
/// duration in the library instead of 0 — a 0 disables gapless/advance/prefetch
/// downstream (the DSD testers' slow-storage libraries: Philippe Landes' 20k
/// DSD tracks). Play-time backfill (resolve_local_track) already repairs it on
/// first play; this fixes the library display up front. `None` for non-DSD, on
/// any parse error, or if even the header read times out.
fn probe_dsd_header_duration_bounded(path: &std::path::Path) -> Option<u64> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "dsf" && ext != "dff" {
        return None;
    }
    let p = path.to_string_lossy().to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let dur = if p.to_ascii_lowercase().ends_with(".dff") {
            crate::audio::dff::parse_dff(&p)
                .ok()
                .and_then(|i| i.duration_ms())
        } else {
            crate::audio::dsf::parse_dsf(&p)
                .ok()
                .and_then(|i| i.duration_ms())
        };
        let _ = tx.send(dur);
    });
    rx.recv_timeout(Duration::from_secs(10))
        .ok()
        .flatten()
        .filter(|&d| d > 0)
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
    /// Paths where the walk itself errored MID-scan (a subfolder that became
    /// unreadable, a transient SMB stall, a nested mount that dropped, a
    /// permission wall). Files below these paths may still exist on disk even
    /// though they are absent from `files` — the post-scan prune must treat
    /// them like `missing_dirs`, otherwise their tracks get silently deleted.
    pub error_dirs: Vec<String>,
    /// One "path: kind — message" line per entry of `missing_dirs`, so the
    /// scan report can tell the user WHY a root was skipped (NotFound = bad
    /// UNC / NAS unmounted, PermissionDenied = no SMB credentials, mapped
    /// drive invisible to a service token, …) instead of burying the reason
    /// in the server log (Alain Bonnel, Windows NAS).
    pub missing_dir_reasons: Vec<String>,
    /// Fichiers audio rencontrés mais non lus, comptés par extension.
    ///
    /// Vide dans l'immense majorité des cas. Quand il ne l'est pas, c'est la
    /// seule chose qui explique à l'utilisateur pourquoi des albums manquent —
    /// sans quoi ils disparaissent en silence (cf `KNOWN_UNREAD_AUDIO`).
    pub skipped_by_ext: std::collections::HashMap<String, usize>,
}

impl ListAudioResult {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.missing_dirs.is_empty()
    }
}

pub fn list_audio_files(dirs: &[String]) -> ListAudioResult {
    list_audio_files_with_excludes(dirs, &[])
}

/// Like [`list_audio_files`], but skips any entry (file or directory subtree)
/// whose full path contains one of `exclude_patterns` (case-insensitive
/// substring — deliberately simple, no glob engine). Patterns come from the
/// `scan_exclude_paths` setting: staging/incoming folders, backup trees, a
/// sibling's library on a shared NAS…
pub fn list_audio_files_with_excludes(
    dirs: &[String],
    exclude_patterns: &[String],
) -> ListAudioResult {
    let extensions: HashSet<&str> = SUPPORTED_EXTENSIONS.iter().copied().collect();
    let excludes: Vec<String> = exclude_patterns
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let skip_set: HashSet<&str> = SKIP_DIRS.iter().copied().collect();

    let mut files = Vec::new();
    let mut skipped_by_ext: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut missing_dirs = Vec::new();
    let mut missing_dir_reasons: Vec<String> = Vec::new();
    let mut error_dirs: Vec<String> = Vec::new();
    // Above this many distinct error scopes the whole root is clearly in
    // trouble (NAS died mid-walk) — protect the entire root instead of
    // accumulating an unbounded list.
    const MAX_ERROR_SCOPES: usize = 50;
    for dir in dirs {
        let normalized = normalize_path(dir);
        let dir_path = std::path::Path::new(&normalized);

        // Probe with read_dir instead of a bare exists(): on Windows a NAS path
        // fails for several distinct reasons that exists() collapses to `false`
        // (silent skip → "scan finds nothing", Alain Bonnel). read_dir surfaces
        // the actual error so the user learns WHY: bad UNC / NAS unmounted, no
        // SMB credentials for this session, and — the common Windows case — a
        // mapped drive (Z:\) invisible to an elevated / service token even
        // though it works in Explorer. La traduction de cette erreur en cause
        // lisible est le seul travail de `scanner::obstacle`.
        if let Err(e) = std::fs::read_dir(dir_path) {
            // La raison est NOMMÉE ici et pas ailleurs : `missing_dir_reasons`
            // est rendu VERBATIM par le client web (SettingsView.svelte). Le
            // `format!("{}: {:?} — {}", …, e.kind(), e)` d'avant y déposait le
            // `Debug` d'un `ErrorKind` — donc `Uncategorized` pour tout errno
            // que Rust ne mappe pas, dont ENODEV. C'est le message que JeromeQ
            // a recopié du fil 1539 sans pouvoir en rien faire (#2357).
            let (motif, message) = crate::scanner::obstacle::obstacle_de_lecture(&normalized, &e);
            warn!(
                dir = %normalized,
                original = %dir,
                error = %e,
                kind = ?e.kind(),
                // L'errno numérique n'était journalisé nulle part : sans lui,
                // un `Uncategorized` dans les journaux reste indéchiffrable
                // même pour nous.
                errno = ?e.raw_os_error(),
                motif = %motif,
                "scan_dir_unreadable — cannot open directory (unreachable NAS, mapped drive not visible to this session, or permission denied), skipping"
            );
            missing_dir_reasons.push(message);
            missing_dirs.push(normalized);
            continue;
        }
        if !dir_path.is_dir() {
            // Ce chemin n'est atteignable qu'en course (la racine a été
            // remplacée entre la sonde `read_dir` et ce test) : `read_dir`
            // rend déjà ENOTDIR sur un fichier. Il n'en poussait pas moins la
            // racine dans le néant — un `warn!` et un `continue`, rien dans
            // `missing_dirs`. Or `missing_dirs` n'est pas qu'un rapport :
            // c'est ce qui déclenche `VerdictPurge::ProtegeIllisible`. Une
            // racine écartée hors de cette liste voyait donc ses pistes
            // supprimées comme si les fichiers avaient disparu (#2356).
            let (motif, message) = crate::scanner::obstacle::pas_un_dossier(&normalized);
            warn!(
                dir = %normalized,
                motif = %motif,
                "scan_dir_not_a_directory — path is not a directory, skipping"
            );
            missing_dir_reasons.push(message);
            missing_dirs.push(normalized);
            continue;
        }

        let mut dir_file_count = 0usize;
        let mut dir_error_count = 0usize;

        let walker = WalkDir::new(&normalized)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                if !excludes.is_empty() {
                    let path_l = e.path().to_string_lossy().to_lowercase();
                    if excludes.iter().any(|x| path_l.contains(x.as_str())) {
                        debug!(path = %e.path().display(), "scan_excluded_by_pattern");
                        return false;
                    }
                }
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
                    // Skip Tune's own streaming/transcode temp files
                    // (tune-stream-*, tune-prefetch-*, tune-tcache-* in %TEMP%):
                    // a library rooted above the temp dir (Frédéric: whole user
                    // profile) otherwise indexes every transcode as a ghost track.
                    if crate::scanner::is_tune_temp_file(entry.path()) {
                        continue;
                    }
                    let path = entry.path();
                    // Count the AUDIO files we walk past, by extension. Until now
                    // a file whose format Tune does not read simply vanished: no
                    // log, no line in the scan report, nothing. The user sees
                    // albums missing with no way to learn why — the `.cue` /
                    // `.mpc` case (Rhorn, #1763), and every future format.
                    //
                    // Only KNOWN_UNREAD_AUDIO is counted, never "every unknown
                    // extension": a music library holds thousands of .jpg, .nfo,
                    // .m3u and .log, and reporting those would bury the one line
                    // that matters under noise nobody can act on.
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        let lower = ext.to_lowercase();
                        if KNOWN_UNREAD_AUDIO.contains(&lower.as_str()) {
                            *skipped_by_ext.entry(lower).or_insert(0) += 1;
                        }
                    }
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
                    // Record WHERE the walk failed so the prune can protect the
                    // subtree: without this, files under an unreadable subfolder
                    // of a perfectly reachable root drop out of the discovered
                    // set and their tracks get deleted from the library. No
                    // path on the error (rare) → protect the whole root.
                    let err_scope = err
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| normalized.clone());
                    if error_dirs.len() >= MAX_ERROR_SCOPES {
                        if !error_dirs.contains(&normalized) {
                            error_dirs.push(normalized.clone());
                        }
                    } else if !error_dirs.contains(&err_scope) {
                        error_dirs.push(err_scope);
                    }
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
        walk_errors = error_dirs.len(),
        "audio_files_listed"
    );
    // Journalisé ici, une seule ligne par scan, et seulement quand il y a
    // quelque chose à dire : c'est la trace qui manquait pour répondre « vos
    // fichiers .mpc ne sont pas lus » au lieu de chercher un bug de scanner.
    if !skipped_by_ext.is_empty() {
        let mut par_ext: Vec<(String, usize)> = skipped_by_ext
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        par_ext.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total: usize = par_ext.iter().map(|(_, n)| n).sum();
        let detail = par_ext
            .iter()
            .map(|(e, n)| format!(".{e}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        warn!(
            total,
            detail = %detail,
            "scan_unsupported_audio_skipped — fichiers audio rencontrés mais non lus par Tune"
        );
    }

    ListAudioResult {
        files,
        missing_dirs,
        error_dirs,
        missing_dir_reasons,
        skipped_by_ext,
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
            let stat_ok = file_meta.is_some();
            let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = file_meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            // Zero-byte "audio" files are aborted copies/downloads, not
            // tracks: don't index a tagless duration-0 ghost, surface them in
            // failed_paths so the report shows what to clean.
            if stat_ok && file_size == 0 {
                warn!(path = %path_str, "scan_file_empty_skipped — zero-byte file (aborted copy?)");
                failed_files
                    .lock()
                    .unwrap()
                    .push((path_str.clone(), "empty file (0 bytes)".into()));
                return ScannedFile {
                    path: path_str,
                    metadata: None,
                    audio_hash: None,
                    file_size,
                    mtime,
                };
            }

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
                    let mut meta = tagless_fallback_no_props(path);
                    // The full tag read timed out, but the tiny DSD header
                    // usually still reads — recover the duration so a slow-storage
                    // DSD track isn't left at 0 in the library (a 0 breaks
                    // gapless/advance/prefetch). Bounded; non-DSD relies on the
                    // play-time backfill.
                    if meta.duration_ms.is_none_or(|d| d == 0) {
                        if let Some(d) = probe_dsd_header_duration_bounded(path) {
                            meta.duration_ms = Some(d);
                        }
                    }
                    (Some(meta), None)
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
    // SCAN_IO_CONCURRENCY). Built once per process and reused by every scan
    // instead of spawning/tearing down 32 OS threads on each scan pass. Falls
    // back to the default rayon pool if it couldn't be built.
    let io_pool = scan_io_pool();

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
                    let stat_ok = file_meta.is_some();
                    let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let mtime = file_meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    // Zero-byte "audio" files are aborted copies/downloads, not
                    // tracks: don't index a tagless duration-0 ghost, surface
                    // them in failed_paths so the report shows what to clean.
                    if stat_ok && file_size == 0 {
                        warn!(path = %path_str, "scan_file_empty_skipped — zero-byte file (aborted copy?)");
                        failed_files
                            .lock()
                            .unwrap()
                            .push((path_str.clone(), "empty file (0 bytes)".into()));
                        return ScannedFile {
                            path: path_str,
                            metadata: None,
                            audio_hash: None,
                            file_size,
                            mtime,
                        };
                    }

                    let (metadata, audio_hash) = match read_file_with_retry(path, with_hash) {
                        Ok((meta, hash)) => (meta, hash),
                        Err(ref reason) if reason == "timeout" => {
                            // Don't drop the file — same fallback as
                            // scan_files_parallel: index it with filename-based
                            // metadata so it still appears in the library.
                            // audio_hash stays None so the next scan re-reads
                            // full tags once storage is responsive.
                            warn!(
                                path = %path_str,
                                timeout_secs = FILE_TIMEOUT.as_secs(),
                                "scan_file_timeout — tag read timed out, indexing with filename metadata"
                            );
                            batch_timeout_counter.fetch_add(1, Ordering::Relaxed);
                            (Some(tagless_fallback_no_props(path)), None)
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
        let batch: Vec<ScannedFile> = match io_pool {
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

    /// Un disque à plateaux ne parallélise pas : 32 lectures concurrentes font
    /// osciller une tête unique entre 32 endroits. Chaque fichier demande deux
    /// déplacements — les tags au début, le hachage de déduplication à 25 % —
    /// donc 64 en vol sur un seul actionneur. Mesuré chez Yacine : 5,8 fichiers
    /// par seconde sur 49 488 fichiers en USB (#1948).
    ///
    /// Fonction pure, donc testable sans `/sys` ni disque : c'est tout l'intérêt
    /// de l'avoir séparée de la sonde système.
    #[test]
    fn un_disque_a_plateaux_reduit_la_concurrence() {
        assert_eq!(concurrence_pour_disque(Some(true)), 4);
    }

    /// Un SSD garde la valeur d'origine, et surtout un type INCONNU aussi :
    /// hors Linux, ou si `/sys` est illisible, on ne dégrade pas tout le monde
    /// par prudence mal placée.
    #[test]
    fn un_ssd_ou_un_type_inconnu_garde_la_valeur_d_origine() {
        assert_eq!(concurrence_pour_disque(Some(false)), SCAN_IO_CONCURRENCY);
        assert_eq!(concurrence_pour_disque(None), SCAN_IO_CONCURRENCY);
    }

    /// La sonde ne doit jamais paniquer ni bloquer : ce n'est qu'une heuristique
    /// de performance, elle ne doit pas pouvoir empêcher un scan.
    #[test]
    fn la_sonde_ne_panique_jamais() {
        let _ = disque_rotatif();
        // Et la valeur qu'elle produit reste dans les bornes utiles.
        let n = concurrence_pour_disque(disque_rotatif());
        assert!((1..=256).contains(&n), "concurrence hors bornes : {n}");
    }

    #[test]
    fn scan_io_concurrency_env_override() {
        // Serialize env mutation and always restore, so this can't race or leak
        // into other tests that read the same variable.
        let key = "TUNE_SCAN_IO_CONCURRENCY";
        let saved = std::env::var(key).ok();

        // Sans variable, c'est le TYPE DE DISQUE qui décide (#1948) — plus la
        // constante. Comparer à la constante ferait passer ce test par chance
        // sur un runner à SSD, et échouer sur une machine à plateaux.
        let sans_variable = concurrence_pour_disque(disque_rotatif());
        unsafe { std::env::remove_var(key) };
        assert_eq!(scan_io_concurrency(), sans_variable);

        unsafe { std::env::set_var(key, "8") };
        assert_eq!(scan_io_concurrency(), 8);

        // Zero, garbage and empty all fall back to the default.
        unsafe { std::env::set_var(key, "0") };
        assert_eq!(scan_io_concurrency(), sans_variable);
        unsafe { std::env::set_var(key, "abc") };
        assert_eq!(scan_io_concurrency(), sans_variable);

        // Over-large is clamped, not honoured verbatim.
        unsafe { std::env::set_var(key, "100000") };
        assert_eq!(scan_io_concurrency(), 256);

        match saved {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn supported_extensions_list() {
        assert!(SUPPORTED_EXTENSIONS.contains(&"flac"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"mp3"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"dsf"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"ape"));
        assert!(!SUPPORTED_EXTENSIONS.contains(&"txt"));
    }

    /// Les deux listes doivent rester DISJOINTES.
    ///
    /// Ajouter un format à `SUPPORTED_EXTENSIONS` sans le retirer de
    /// `KNOWN_UNREAD_AUDIO` produirait un rapport de scan qui annonce comme
    /// « non lus » des fichiers pourtant indexés — pire qu'un silence, puisque
    /// l'utilisateur irait chercher un problème inexistant.
    #[test]
    fn unread_list_never_overlaps_supported() {
        for e in KNOWN_UNREAD_AUDIO {
            assert!(
                !SUPPORTED_EXTENSIONS.contains(e),
                "{e} est à la fois lu et annoncé comme non lu"
            );
        }
    }

    /// Ce que la liste doit couvrir, et ce qu'elle ne doit surtout pas.
    #[test]
    fn unread_list_targets_audio_only() {
        // Les formats réclamés sur le forum (Rhorn, #1763).
        assert!(KNOWN_UNREAD_AUDIO.contains(&"mpc"));
        assert!(KNOWN_UNREAD_AUDIO.contains(&"cue"));
        // Le bruit d'une bibliothèque musicale ne doit JAMAIS y figurer :
        // compter les pochettes et les fichiers de log noierait le seul
        // renseignement exploitable.
        for noise in ["jpg", "png", "nfo", "m3u", "log", "txt", "accurip", "pdf"] {
            assert!(
                !KNOWN_UNREAD_AUDIO.contains(&noise),
                "{noise} n'est pas de l'audio et polluerait le rapport"
            );
        }
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
        assert_eq!(result.missing_dir_reasons.len(), 1);
        assert!(result.error_dirs.is_empty());
    }

    /// La ligne que voit l'utilisateur — celle que `SettingsView.svelte` rend
    /// verbatim — ne doit contenir aucun nom de variante de `std::io::ErrorKind`
    /// ni de `os error N`. C'est tout le sujet de #2357 : JeromeQ a recopié
    /// `Uncategorized — No such device (os error 19)` sans pouvoir en rien
    /// faire. Ici, sur une racine simplement absente, le rendu partait déjà en
    /// `NotFound`.
    #[test]
    fn la_raison_rendue_a_l_ecran_est_une_phrase_pas_un_errno() {
        let result = list_audio_files(&["/tmp/nonexistent_tune_test_dir".into()]);
        let raison = &result.missing_dir_reasons[0];
        for mot in ["NotFound", "Uncategorized", "os error"] {
            assert!(
                !raison.contains(mot),
                "jargon « {mot} » rendu à l'écran : {raison:?}"
            );
        }
        assert!(
            raison.contains("/tmp/nonexistent_tune_test_dir"),
            "le chemin fautif doit être nommé : {raison:?}"
        );
        let bas = raison.to_lowercase();
        assert!(
            bas.contains("n'existe pas") || bas.contains("introuvable") || bas.contains("absent"),
            "la cause doit être dite en français : {raison:?}"
        );
    }

    /// #2356, seconde face : une racine écartée ne doit JAMAIS l'être en
    /// silence. Un chemin configuré qui n'est pas un dossier était sauté avec
    /// un simple `warn!` : absent de `missing_dirs`, il n'apparaissait ni dans
    /// le rapport de scan, ni dans le garde-fou de purge — ses pistes étaient
    /// donc supprimées comme si les fichiers avaient disparu.
    #[test]
    fn une_racine_qui_n_est_pas_un_dossier_est_signalee_et_non_sautee() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_racine_fichier_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let fichier = base.join("ceci_est_un_fichier.txt");
        std::fs::write(&fichier, b"x").unwrap();

        let result = list_audio_files(&[fichier.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            result.missing_dirs.len(),
            1,
            "racine sautée en silence : missing_dirs = {:?}",
            result.missing_dirs
        );
        assert_eq!(result.missing_dir_reasons.len(), 1);
        let raison = &result.missing_dir_reasons[0];
        for mot in ["NotFound", "Uncategorized", "NotADirectory", "os error"] {
            assert!(!raison.contains(mot), "jargon rendu à l'écran : {raison:?}");
        }
        assert!(
            raison.to_lowercase().contains("dossier"),
            "la cause doit être dite : {raison:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_error_subdir_recorded_in_error_dirs() {
        use std::os::unix::fs::PermissionsExt;
        // NOT under temp_dir(): is_tune_temp_file() skips every file inside
        // the system temp dir, which would empty the walk result.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_error_dirs_test");
        let locked = base.join("locked");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("hidden.flac"), b"x").unwrap();
        std::fs::write(base.join("visible.flac"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root (some CI containers): chmod 000 doesn't block the
        // walk, the scenario can't be reproduced — skip.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&base);

        // The reachable file is still scanned; the root is NOT "missing"; the
        // unreadable subtree is reported so the prune can protect it instead
        // of deleting its tracks.
        assert_eq!(result.files.len(), 1);
        assert!(result.missing_dirs.is_empty());
        assert!(
            result.error_dirs.iter().any(|d| d.contains("locked")),
            "error_dirs = {:?}",
            result.error_dirs
        );
    }

    #[test]
    fn exclude_patterns_prune_files_and_subtrees() {
        // NOT under temp_dir(): is_tune_temp_file() skips everything there.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_excludes_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("keep")).unwrap();
        std::fs::create_dir_all(base.join("Incoming")).unwrap();
        std::fs::write(base.join("keep/a.flac"), b"x").unwrap();
        std::fs::write(base.join("Incoming/b.flac"), b"x").unwrap();

        let root = base.to_string_lossy().to_string();
        let all = list_audio_files(&[root.clone()]);
        assert_eq!(all.files.len(), 2);

        // Case-insensitive substring match prunes the whole subtree.
        let filtered = list_audio_files_with_excludes(&[root], &["incoming".to_string()]);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(filtered.files.len(), 1, "files = {:?}", filtered.files);
        assert!(filtered.files[0].to_string_lossy().contains("keep"));
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
