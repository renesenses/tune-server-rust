use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

const DEBOUNCE_MS: u64 = 2000;

static SUPPORTED_EXTENSIONS: &[&str] = &[
    ".flac", ".mp3", ".m4a", ".ogg", ".opus", ".wav", ".aiff", ".aif", ".wma", ".dsf", ".dff",
    ".ape", ".wv", ".alac",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

pub struct FileSystemWatcher {
    music_dirs: Vec<String>,
    event_tx: mpsc::Sender<Vec<FileChange>>,
    cancel: Arc<Mutex<bool>>,
}

impl FileSystemWatcher {
    pub fn new(music_dirs: Vec<String>, event_tx: mpsc::Sender<Vec<FileChange>>) -> Self {
        Self {
            music_dirs,
            event_tx,
            cancel: Arc::new(Mutex::new(false)),
        }
    }

    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let dirs = self.music_dirs.clone();
        let tx = self.event_tx.clone();
        let cancel = self.cancel.clone();

        info!(dirs = ?dirs, "filesystem_watcher_started");

        tokio::spawn(async move {
            Self::poll_loop(dirs, tx, cancel).await;
        })
    }

    pub async fn stop(&self) {
        *self.cancel.lock().await = true;
        info!("filesystem_watcher_stopped");
    }

    async fn poll_loop(
        dirs: Vec<String>,
        tx: mpsc::Sender<Vec<FileChange>>,
        cancel: Arc<Mutex<bool>>,
    ) {
        let mut known: HashMap<String, u64> = HashMap::new();

        // Initial scan
        for dir in &dirs {
            Self::scan_dir(dir, &mut known);
        }

        loop {
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;

            if *cancel.lock().await {
                break;
            }

            let mut changes = Vec::new();
            let mut current: HashMap<String, u64> = HashMap::new();

            for dir in &dirs {
                Self::scan_dir(dir, &mut current);
            }

            // Detect added/modified
            for (path, mtime) in &current {
                match known.get(path) {
                    None => changes.push(FileChange {
                        path: path.clone(),
                        kind: ChangeKind::Added,
                    }),
                    Some(old_mtime) if old_mtime != mtime => changes.push(FileChange {
                        path: path.clone(),
                        kind: ChangeKind::Modified,
                    }),
                    _ => {}
                }
            }

            // Detect deleted
            for path in known.keys() {
                if !current.contains_key(path) {
                    changes.push(FileChange {
                        path: path.clone(),
                        kind: ChangeKind::Deleted,
                    });
                }
            }

            if !changes.is_empty() {
                debug!(count = changes.len(), "filesystem_changes_detected");
                let _ = tx.send(changes).await;
            }

            known = current;
        }
    }

    fn scan_dir(dir: &str, map: &mut HashMap<String, u64>) {
        let path = Path::new(dir);
        if !path.exists() {
            warn!(dir, "watch_dir_not_found");
            return;
        }

        let walker = walkdir::WalkDir::new(path)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok());

        for entry in walker {
            if !entry.file_type().is_file() {
                continue;
            }

            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e.to_lowercase()));

            if let Some(ref ext) = ext {
                if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                    continue;
                }
            } else {
                continue;
            }

            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            map.insert(entry.path().to_string_lossy().to_string(), mtime);
        }
    }

    pub fn is_supported_extension(path: &str) -> bool {
        let lower = path.to_lowercase();
        SUPPORTED_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions() {
        assert!(FileSystemWatcher::is_supported_extension("song.flac"));
        assert!(FileSystemWatcher::is_supported_extension("track.MP3"));
        assert!(FileSystemWatcher::is_supported_extension(
            "/music/album/01.m4a"
        ));
        assert!(!FileSystemWatcher::is_supported_extension("readme.txt"));
        assert!(!FileSystemWatcher::is_supported_extension("cover.jpg"));
    }

    #[test]
    fn change_kind_eq() {
        assert_eq!(ChangeKind::Added, ChangeKind::Added);
        assert_ne!(ChangeKind::Added, ChangeKind::Deleted);
    }

    #[tokio::test]
    async fn watcher_stop() {
        let (tx, _rx) = mpsc::channel(16);
        let watcher = FileSystemWatcher::new(vec![], tx);
        watcher.stop().await;
        assert!(*watcher.cancel.lock().await);
    }

    #[test]
    fn scan_nonexistent_dir() {
        let mut map = HashMap::new();
        FileSystemWatcher::scan_dir("/nonexistent/path/abc123", &mut map);
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn watcher_detects_tempdir_changes() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_string_lossy().to_string();

        let (tx, mut rx) = mpsc::channel(16);
        let watcher = FileSystemWatcher::new(vec![dir_path.clone()], tx);
        let handle = watcher.start();

        // Wait for initial scan
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Create a file
        let test_file = dir.path().join("test.flac");
        std::fs::write(&test_file, b"fake flac data").unwrap();

        // Wait for detection
        let changes = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(!changes.is_empty());
        assert_eq!(changes[0].kind, ChangeKind::Added);
        assert!(changes[0].path.contains("test.flac"));

        watcher.stop().await;
        let _ = handle.await;
    }
}
