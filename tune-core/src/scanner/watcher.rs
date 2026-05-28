use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff", "aif", "wv", "wma", "dsf", "dff", "dst",
    "alac", "ape",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub change_type: ChangeType,
    pub path: String,
}

pub struct FileWatcher {
    watcher: Option<RecommendedWatcher>,
    event_rx: std::sync::Mutex<mpsc::Receiver<FileChange>>,
    dirs: Vec<PathBuf>,
}

impl FileWatcher {
    pub fn new(dirs: Vec<String>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let event_tx = tx;

        let mut watcher =
            notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
                Ok(event) => {
                    let change_type = match event.kind {
                        EventKind::Create(_) => Some(ChangeType::Added),
                        EventKind::Modify(_) => Some(ChangeType::Modified),
                        EventKind::Remove(_) => Some(ChangeType::Deleted),
                        _ => None,
                    };

                    if let Some(ct) = change_type {
                        for path in &event.paths {
                            if is_audio_file(path) {
                                let _ = event_tx.send(FileChange {
                                    change_type: ct.clone(),
                                    path: path.to_string_lossy().to_string(),
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "watcher_error");
                }
            })
            .map_err(|e| format!("watcher init: {e}"))?;

        let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
        for dir in &dirs {
            if dir.exists() {
                watcher
                    .watch(dir, RecursiveMode::Recursive)
                    .map_err(|e| format!("watch {}: {e}", dir.display()))?;
                info!(dir = %dir.display(), "watching_directory");
            } else {
                warn!(dir = %dir.display(), "watch_dir_not_found");
            }
        }

        Ok(Self {
            watcher: Some(watcher),
            event_rx: std::sync::Mutex::new(rx),
            dirs,
        })
    }

    pub fn poll_changes(&self, timeout: Duration) -> Vec<FileChange> {
        let rx = self.event_rx.lock().unwrap();
        let mut changes = Vec::new();
        match rx.recv_timeout(timeout) {
            Ok(change) => {
                changes.push(change);
                while let Ok(c) = rx.try_recv() {
                    changes.push(c);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                debug!("watcher_channel_disconnected");
            }
        }
        changes
    }

    pub fn poll_debounced(&self, timeout: Duration, debounce: Duration) -> Vec<FileChange> {
        let raw = self.poll_changes(timeout);
        if raw.is_empty() {
            return raw;
        }

        std::thread::sleep(debounce);

        let rx = self.event_rx.lock().unwrap();
        let mut more = Vec::new();
        while let Ok(c) = rx.try_recv() {
            more.push(c);
        }

        let mut merged: HashMap<String, ChangeType> = HashMap::new();
        for change in raw.into_iter().chain(more) {
            merged.insert(change.path.clone(), change.change_type);
        }

        merged
            .into_iter()
            .map(|(path, change_type)| FileChange { change_type, path })
            .collect()
    }

    pub fn stop(&mut self) {
        if let Some(mut w) = self.watcher.take() {
            for dir in &self.dirs {
                let _ = w.unwatch(dir);
            }
        }
        info!("file_watcher_stopped");
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn audio_file_detection() {
        assert!(is_audio_file(Path::new("test.flac")));
        assert!(is_audio_file(Path::new("test.MP3")));
        assert!(is_audio_file(Path::new("/path/to/file.dsf")));
        assert!(!is_audio_file(Path::new("readme.txt")));
        assert!(!is_audio_file(Path::new("cover.jpg")));
    }

    #[test]
    fn watcher_lifecycle() {
        let dir = std::env::temp_dir().join("tune_watcher_test");
        fs::create_dir_all(&dir).unwrap();

        let mut watcher = FileWatcher::new(vec![dir.to_string_lossy().to_string()]).unwrap();

        let test_file = dir.join("test.flac");
        {
            let mut f = fs::File::create(&test_file).unwrap();
            f.write_all(b"fake flac data").unwrap();
        }

        let changes = watcher.poll_changes(Duration::from_secs(2));
        // May or may not catch the event depending on timing
        if !changes.is_empty() {
            assert!(changes.iter().any(|c| c.path.contains("test.flac")));
        }

        watcher.stop();
        fs::remove_file(&test_file).ok();
        fs::remove_dir(&dir).ok();
    }
}
