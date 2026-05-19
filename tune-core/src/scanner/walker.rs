use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use tracing::info;
use walkdir::WalkDir;

use crate::metadata::{read_metadata, TrackMetadata};
use super::hasher::compute_audio_hash;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff", "aif",
    "wv", "wma", "dsf", "dff", "dst", "alac", "ape",
];

const SKIP_DIRS: &[&str] = &[
    "duplicates", ".tune", ".Spotlight-V100", ".Trashes",
    "@eaDir", "#recycle", ".DS_Store", "$RECYCLE.BIN",
    "System Volume Information",
];

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
    pub hash_ok: usize,
}

pub fn list_audio_files(dirs: &[String]) -> Vec<PathBuf> {
    let extensions: HashSet<&str> = SUPPORTED_EXTENSIONS.iter().copied().collect();
    let skip_set: HashSet<&str> = SKIP_DIRS.iter().copied().collect();

    let mut files = Vec::new();
    for dir in dirs {
        let walker = WalkDir::new(dir)
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

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if extensions.contains(ext.to_lowercase().as_str()) {
                    files.push(path.to_path_buf());
                }
            }
        }
    }

    info!(count = files.len(), dirs = dirs.len(), "audio_files_listed");
    files
}

pub fn scan_files_parallel(
    files: &[PathBuf],
    with_hash: bool,
    progress_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (Vec<ScannedFile>, ScanStats) {
    let counter = AtomicUsize::new(0);
    let total = files.len();

    let results: Vec<ScannedFile> = files
        .par_iter()
        .map(|path| {
            let idx = counter.fetch_add(1, Ordering::Relaxed);
            if let Some(ref cb) = progress_callback {
                if idx % 100 == 0 {
                    cb(idx, total);
                }
            }

            let path_str = path.to_string_lossy().to_string();

            let file_size = path.metadata().map(|m| m.len()).unwrap_or(0);
            let mtime = path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let metadata = read_metadata(path);

            let audio_hash = if with_hash {
                compute_audio_hash(path)
            } else {
                None
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

    let stats = ScanStats {
        total_files: results.len(),
        metadata_ok: results.iter().filter(|f| f.metadata.is_some()).count(),
        metadata_failed: results.iter().filter(|f| f.metadata.is_none()).count(),
        hash_ok: results.iter().filter(|f| f.audio_hash.is_some()).count(),
    };

    info!(
        total = stats.total_files,
        metadata_ok = stats.metadata_ok,
        metadata_failed = stats.metadata_failed,
        "parallel_scan_complete"
    );

    (results, stats)
}

pub fn scan_directories(
    dirs: &[String],
    with_hash: bool,
    progress_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (Vec<ScannedFile>, ScanStats) {
    let files = list_audio_files(dirs);
    scan_files_parallel(&files, with_hash, progress_callback)
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
        let files = list_audio_files(&["/tmp/nonexistent_tune_test_dir".into()]);
        assert!(files.is_empty());
    }

    #[test]
    fn scan_empty() {
        let (results, stats) = scan_directories(&[], false, None);
        assert!(results.is_empty());
        assert_eq!(stats.total_files, 0);
    }
}
