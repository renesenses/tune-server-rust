use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

/// Poll interval for directories on network mounts. notify's native backends
/// (FSEvents/inotify/ReadDirectoryChangesW) receive NOTHING for changes made
/// by other machines on an SMB/NFS share — the watcher looked alive but was
/// deaf for the most common NAS setup. Polling stats the WHOLE tree every
/// interval: on a large SMB library one sweep alone can take minutes
/// (Pierre M: 6 min 43 for the baseline walk of K:\), so 120 s would have
/// kept the NAS under permanent scan. 15 min keeps the sweep an occasional
/// background cost while still surfacing remote changes without a rescan.
const NETWORK_POLL_INTERVAL: Duration = Duration::from_secs(900);

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
    /// Secondary watcher for network mounts, where the native backend gets no
    /// events for remote changes. Built lazily, only when such a dir exists.
    poll_watcher: Option<PollWatcher>,
    event_tx: mpsc::Sender<FileChange>,
    event_rx: std::sync::Mutex<mpsc::Receiver<FileChange>>,
    /// Dirs currently watched by the native watcher.
    dirs: Vec<PathBuf>,
    /// Dirs currently watched by the poll watcher (network mounts).
    poll_dirs: Vec<PathBuf>,
    /// Requested dirs not currently watched (missing/unmounted at the time).
    /// `ensure_watches` retries them so a NAS mounted after boot — or
    /// remounted after a drop — gets picked up without a restart.
    pending: Vec<PathBuf>,
}

/// Shared notify event handler: translate raw events into FileChange messages.
fn make_event_handler(event_tx: mpsc::Sender<FileChange>) -> impl Fn(Result<Event, notify::Error>) {
    move |res: Result<Event, notify::Error>| match res {
        Ok(event) => {
            let change_type = match event.kind {
                EventKind::Create(_) => Some(ChangeType::Added),
                // Only treat data/content changes and renames as
                // modifications.  Ignore metadata-only changes
                // (xattr, Finder info, inode meta) — on macOS,
                // Spotlight indexing writes extended attributes to
                // audio files after they are read, which fires
                // Modify(Metadata(Extended)) events.  Treating
                // those as content changes creates an infinite
                // read→xattr→event→read loop (seen on Ventura).
                EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Modify(ModifyKind::Any) => Some(ChangeType::Modified),
                EventKind::Modify(ModifyKind::Metadata(_))
                | EventKind::Modify(ModifyKind::Other) => None,
                EventKind::Remove(_) => Some(ChangeType::Deleted),
                _ => None,
            };

            if let Some(ct) = change_type {
                for path in &event.paths {
                    if is_audio_file(path) && !super::is_tune_temp_file(path) {
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
    }
}

impl FileWatcher {
    pub fn new(dirs: Vec<String>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();

        let watcher = notify::recommended_watcher(make_event_handler(tx.clone()))
            .map_err(|e| format!("watcher init: {e}"))?;

        // Normalize like every other consumer of music_dirs (trailing slashes,
        // Windows separators) — the raw settings values were passed through
        // before, so a dir stored as "D:/Musique/" was watched under a path
        // spelling the rest of the pipeline never uses.
        let requested: Vec<PathBuf> = dirs
            .iter()
            .map(|d| PathBuf::from(super::walker::normalize_path(d)))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();

        let mut this = Self {
            watcher: Some(watcher),
            poll_watcher: None,
            event_tx: tx,
            event_rx: std::sync::Mutex::new(rx),
            dirs: Vec::new(),
            poll_dirs: Vec::new(),
            pending: requested.clone(),
        };
        this.ensure_watches();

        if this.dirs.is_empty() && this.poll_dirs.is_empty() && !requested.is_empty() {
            return Err("no music directory could be watched".to_string());
        }
        Ok(this)
    }

    /// Try to watch every pending dir, and detect watched dirs whose mount
    /// vanished. Called at startup and periodically from the watch loop, so a
    /// NAS mounted late — or remounted after a drop — resumes live updates
    /// without a server restart. Watch per-directory, resiliently: one
    /// unreadable or unmounted dir must not kill watching for the others (it
    /// aborted the whole watcher before).
    pub fn ensure_watches(&mut self) {
        // Watched dirs whose mount disappeared go back to pending; their
        // native watch is dead even if the mount comes back under the path.
        let mut still_watched = Vec::new();
        for dir in std::mem::take(&mut self.dirs) {
            if std::fs::read_dir(&dir).is_ok() {
                still_watched.push(dir);
            } else {
                warn!(dir = %dir.display(), "watch_dir_lost — unmounted or unreadable, will re-watch when it returns");
                if let Some(w) = self.watcher.as_mut() {
                    let _ = w.unwatch(&dir);
                }
                self.pending.push(dir);
            }
        }
        self.dirs = still_watched;
        let mut still_polled = Vec::new();
        for dir in std::mem::take(&mut self.poll_dirs) {
            if std::fs::read_dir(&dir).is_ok() {
                still_polled.push(dir);
            } else {
                warn!(dir = %dir.display(), "watch_dir_lost — unmounted or unreadable, will re-watch when it returns");
                if let Some(w) = self.poll_watcher.as_mut() {
                    let _ = w.unwatch(&dir);
                }
                self.pending.push(dir);
            }
        }
        self.poll_dirs = still_polled;

        // Retry pending dirs.
        for dir in std::mem::take(&mut self.pending) {
            if std::fs::read_dir(&dir).is_err() {
                self.pending.push(dir);
                continue;
            }
            if is_network_path(&dir) {
                // Native backends receive no events for changes made by other
                // machines on an SMB/NFS share — poll instead.
                if self.poll_watcher.is_none() {
                    match PollWatcher::new(
                        make_event_handler(self.event_tx.clone()),
                        Config::default().with_poll_interval(NETWORK_POLL_INTERVAL),
                    ) {
                        Ok(pw) => self.poll_watcher = Some(pw),
                        Err(e) => {
                            warn!(error = %e, "poll_watcher_init_failed — falling back to native watch");
                        }
                    }
                }
                if let Some(pw) = self.poll_watcher.as_mut() {
                    match pw.watch(&dir, RecursiveMode::Recursive) {
                        Ok(()) => {
                            info!(dir = %dir.display(), interval_secs = NETWORK_POLL_INTERVAL.as_secs(), "watching_directory_poll — network mount, using polling");
                            self.poll_dirs.push(dir);
                            continue;
                        }
                        Err(e) => {
                            warn!(dir = %dir.display(), error = %e, "poll_watch_failed — falling back to native watch");
                        }
                    }
                }
            }
            if let Some(w) = self.watcher.as_mut() {
                match w.watch(&dir, RecursiveMode::Recursive) {
                    Ok(()) => {
                        info!(dir = %dir.display(), "watching_directory");
                        self.dirs.push(dir);
                    }
                    Err(e) => {
                        warn!(dir = %dir.display(), error = %e, "watch_dir_failed — skipping, other dirs still watched");
                        self.pending.push(dir);
                    }
                }
            }
        }
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
        if let Some(mut w) = self.poll_watcher.take() {
            for dir in &self.poll_dirs {
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

/// Whether a path lives on a network filesystem (SMB/CIFS/NFS/WebDAV/AFP,
/// FUSE-backed remotes, Windows UNC or mapped network drives). Native watch
/// backends are deaf to remote changes on those — the caller polls instead.
#[cfg(target_os = "macos")]
fn is_network_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    let fstype = unsafe { std::ffi::CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    let fstype = fstype.to_string_lossy().to_lowercase();
    matches!(
        fstype.as_str(),
        "smbfs" | "nfs" | "afpfs" | "webdav" | "cifs"
    ) || fstype.starts_with("fuse")
}

#[cfg(target_os = "linux")]
fn is_network_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    // Magic numbers from linux/magic.h.
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const SMB2_MAGIC_NUMBER: i64 = 0xFE534D42;
    const CIFS_MAGIC_NUMBER: i64 = 0xFF534D42;
    const FUSE_SUPER_MAGIC: i64 = 0x65735546;
    const NCP_SUPER_MAGIC: i64 = 0x564C;
    const CODA_SUPER_MAGIC: i64 = 0x73757245;
    matches!(
        buf.f_type as i64,
        NFS_SUPER_MAGIC
            | SMB_SUPER_MAGIC
            | SMB2_MAGIC_NUMBER
            | CIFS_MAGIC_NUMBER
            | FUSE_SUPER_MAGIC
            | NCP_SUPER_MAGIC
            | CODA_SUPER_MAGIC
    )
}

#[cfg(windows)]
fn is_network_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let s = path.as_os_str().to_string_lossy();
    // UNC share: \\server\share\...
    if s.starts_with("\\\\") {
        return true;
    }
    // Mapped drive letter: ask Windows for the drive type of "X:\".
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
        }
        const DRIVE_REMOTE: u32 = 4;
        let root: Vec<u16> = std::ffi::OsString::from(format!("{}:\\", s.chars().next().unwrap()))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        return unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE;
    }
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn is_network_path(_path: &Path) -> bool {
    false
}

fn is_audio_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        let ext = e.to_lowercase();
        // Single source of truth with the walker. "iso" is excluded here:
        // ISO SACD requires the DSF-extraction step that only the full
        // directory walk performs — a raw .iso fed to the watcher pipeline
        // would just fail tag reading. (The old duplicated list had already
        // drifted and was missing "iso" only by accident.)
        ext != "iso" && super::walker::SUPPORTED_EXTENSIONS.contains(&ext.as_str())
    })
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
        let dir = tempfile::TempDir::new().unwrap();

        let mut watcher = FileWatcher::new(vec![dir.path().to_string_lossy().to_string()]).unwrap();

        let test_file = dir.path().join("test.flac");
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
    }
}
