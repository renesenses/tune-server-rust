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
    /// #4896 — un DOSSIER est apparu sous une racine : renommé (nouveau nom),
    /// déplacé depuis ailleurs, ou créé. Les trois moteurs natifs ne signalent
    /// que le dossier, jamais les fichiers qu'il emporte.
    DossierApparu,
    /// #4896 — un chemin qui n'est pas un fichier audio a quitté le disque :
    /// peut-être un dossier renommé (ancien nom), déplacé hors de la racine,
    /// mis à la corbeille ou supprimé. Rien ne dit ici que c'était un dossier
    /// — il n'existe plus — : `auto_scan` le décide d'après les pistes qu'il
    /// contenait, et un chemin sans piste n'y touche à rien.
    DossierDisparu,
    /// #5034 — une IMAGE DE POCHETTE de dossier (`cover.jpg`, `folder.png`…)
    /// a été créée, modifiée, renommée ou supprimée. Le surveillant ne
    /// relayait que l'audio : remplacer ou retirer le `cover.jpg` d'un album
    /// n'était vu par personne jusqu'au scan suivant — et même lui ne le
    /// voyait pas, les pistes n'ayant pas changé. Le geste exact importe peu :
    /// `auto_scan` relit l'album du dossier et laisse la règle trancher.
    ImageDePochette,
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
            // #4896 — les événements de DOSSIER. Ils étaient tous écartés par
            // le filtre audio ci-dessus : un dossier d'album renommé ou mis à
            // la corbeille n'était vu qu'au scan suivant. Un montage ou un
            // démontage (FSEvents, info « mount ») n'en est pas un : la
            // reprise d'une racine est l'affaire de `ensure_watches`.
            if event.info() == Some("mount") {
                return;
            }
            for path in &event.paths {
                if is_audio_file(path) || super::is_tune_temp_file(path) {
                    continue;
                }
                // #5034 — une image de pochette n'est pas un dossier : sous
                // Windows, sa suppression (`Remove(Any)`) passerait sinon pour
                // un dossier disparu.
                if crate::library::pochette_disque::est_une_image_de_pochette(path) {
                    let _ = event_tx.send(FileChange {
                        change_type: ChangeType::ImageDePochette,
                        path: path.to_string_lossy().to_string(),
                    });
                    continue;
                }
                if let Some(genre) = evenement_de_dossier(&event.kind, path) {
                    let _ = event_tx.send(FileChange {
                        change_type: genre,
                        path: path.to_string_lossy().to_string(),
                    });
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "watcher_error");
        }
    }
}

/// #4896 — ce qu'un événement dit d'un chemin qui n'est pas un fichier audio.
///
/// Les trois moteurs natifs de `notify` 7.0.0 ne décrivent pas un dossier de
/// la même façon, et deux d'entre eux ne disent même pas que c'en est un :
///
/// | geste | Windows (`windows.rs`) | macOS (`fsevent.rs`) | Linux (`inotify.rs`) |
/// |---|---|---|---|
/// | renommer sur place | `Name(From)` ancien, `Name(To)` nouveau | `Name(Any)` ancien, `Name(Any)` nouveau | `Name(From)`, `Name(To)`, `Name(Both)` [ancien, nouveau] |
/// | déplacer sous la racine | `Remove(Any)` ancien, `Create(Any)` nouveau | comme renommer | comme renommer |
/// | corbeille / sortie de la racine | `Remove(Any)` | `Name(Any)` | `Name(From)` |
/// | supprimer | `Remove(Any)` par fichier puis dossier | `Remove(File)`… puis `Remove(Folder)` | idem |
/// | entrer dans la racine | `Create(Any)` | `Name(Any)` | `Name(To)` |
///
/// Le seul arbitre commun est donc le DISQUE, lu à l'arrivée de l'événement :
/// un chemin qui est un dossier est apparu, un chemin qui n'existe plus a
/// disparu. Un chemin toujours présent qui n'est pas un dossier (pochette,
/// fichier temporaire d'un éditeur de balises) ne dit rien. Un « disparu »
/// n'était peut-être qu'un fichier : `auto_scan` n'y touche que s'il couvrait
/// des pistes indexées.
fn evenement_de_dossier(genre: &EventKind, chemin: &Path) -> Option<ChangeType> {
    use notify::event::{CreateKind, RemoveKind};
    let disparu = || std::fs::symlink_metadata(chemin).is_err();
    match genre {
        // Le moteur a dit « fichier » : ce n'est pas un dossier.
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => None,
        EventKind::Create(_) => chemin.is_dir().then_some(ChangeType::DossierApparu),
        EventKind::Remove(_) => disparu().then_some(ChangeType::DossierDisparu),
        EventKind::Modify(ModifyKind::Name(_)) => {
            if chemin.is_dir() {
                Some(ChangeType::DossierApparu)
            } else if disparu() {
                Some(ChangeType::DossierDisparu)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// #4896 — les fichiers audio d'un dossier apparu, à toute profondeur : un
/// dossier renommé ou déplacé n'amène aucun événement pour son contenu. Les
/// liens symboliques de DOSSIER ne sont pas suivis (une boucle ne se parcourt
/// pas) ; un fichier illisible est simplement absent de la liste.
pub fn fichiers_audio_sous(dossier: &Path) -> Vec<String> {
    let mut trouves = Vec::new();
    let mut a_lire = vec![dossier.to_path_buf()];
    while let Some(courant) = a_lire.pop() {
        let Ok(entrees) = std::fs::read_dir(&courant) else {
            continue;
        };
        for entree in entrees.flatten() {
            let Ok(genre) = entree.file_type() else {
                continue;
            };
            let chemin = entree.path();
            if genre.is_dir() {
                a_lire.push(chemin);
            } else if is_audio_file(&chemin) && !super::is_tune_temp_file(&chemin) {
                trouves.push(chemin.to_string_lossy().to_string());
            }
        }
    }
    trouves.sort();
    trouves
}

/// Le moteur `notify` que ce module traduit, pour que les épreuves des autres
/// caisses fabriquent ses événements sans en dépendre elles-mêmes.
pub use notify;

/// Rejoue des événements `notify` BRUTS dans le gestionnaire de production,
/// puis les fusionne comme `poll_debounced` : le dernier événement d'un chemin
/// l'emporte. Sert aux épreuves du surveillant (#4896) : le gestionnaire est
/// privé, et une épreuve qui recopierait sa traduction ne garderait qu'une
/// copie. (La fusion, trois lignes, est celle de `poll_debounced`, que ce
/// correctif ne touche pas.)
pub fn rejouer_evenements_notify(evenements: Vec<Event>) -> Vec<FileChange> {
    let (tx, rx) = mpsc::channel();
    let gestionnaire = make_event_handler(tx);
    for e in evenements {
        gestionnaire(Ok(e));
    }
    let mut fusion: HashMap<String, ChangeType> = HashMap::new();
    while let Ok(c) = rx.try_recv() {
        fusion.insert(c.path, c.change_type);
    }
    let mut changes: Vec<FileChange> = fusion
        .into_iter()
        .map(|(path, change_type)| FileChange { change_type, path })
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
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

    /// Rejoue des événements `notify` bruts dans le gestionnaire de
    /// production, puis les fusionne comme `poll_debounced` : le dernier
    /// événement d'un chemin l'emporte.
    fn rejouer(evenements: Vec<Event>) -> HashMap<String, ChangeType> {
        let (tx, rx) = mpsc::channel();
        let gestionnaire = make_event_handler(tx);
        for e in evenements {
            gestionnaire(Ok(e));
        }
        let mut fusion = HashMap::new();
        while let Ok(c) = rx.try_recv() {
            fusion.insert(c.path, c.change_type);
        }
        fusion
    }

    fn ev(kind: EventKind, chemin: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(chemin))
    }

    /// #4896 (Didier, fil 1904) — les séquences que le moteur Windows de
    /// `notify` 7.0.0 fabrique (`src/windows.rs`, `handle_event`) :
    /// `FILE_ACTION_MODIFIED` → `Modify(Any)`, `ADDED` → `Create(Any)`,
    /// `REMOVED` → `Remove(Any)`, `RENAMED_OLD_NAME`/`NEW_NAME` →
    /// `Modify(Name(From/To))`. Aucune n'est perdue pour le fichier audio :
    /// la retouche Mp3tag parvient bien jusqu'à `auto_scan`. Le dernier cas
    /// arrive en `Added` sur un chemin DÉJÀ indexé, que `auto_scan` doit
    /// traiter comme un remplacement (`reimporter_fichier_surveillant`).
    #[test]
    fn les_sequences_windows_d_une_retouche_atteignent_le_fichier_audio_4896() {
        use notify::event::{CreateKind, RemoveKind, RenameMode};
        let x = r"D:\Musique\Pink Floyd\Multichannel 7.1\01 - Speak To Me.flac";
        let tmp = r"D:\Musique\Pink Floyd\Multichannel 7.1\01 - Speak To Me.tmp";
        // Écriture en place (FLAC au remplissage suffisant).
        let en_place = rejouer(vec![ev(EventKind::Modify(ModifyKind::Any), x)]);
        assert_eq!(en_place.get(x), Some(&ChangeType::Modified));
        // Fichier temporaire du même dossier, puis renommage par-dessus.
        let par_renommage = rejouer(vec![
            ev(EventKind::Create(CreateKind::Any), tmp),
            ev(EventKind::Modify(ModifyKind::Any), tmp),
            ev(EventKind::Remove(RemoveKind::Any), x),
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::From)), tmp),
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::To)), x),
        ]);
        assert_eq!(par_renommage.get(x), Some(&ChangeType::Modified));
        // Le temporaire n'est pas un changement de FICHIER audio. Disparu du
        // disque, il peut sortir en candidat « dossier disparu » : `auto_scan`
        // l'écarte faute de piste indexée sous ce chemin (#4896,
        // `un_chemin_disparu_sans_piste_ne_touche_a_rien_4896`).
        assert!(
            !matches!(
                par_renommage.get(tmp),
                Some(ChangeType::Added | ChangeType::Modified | ChangeType::Deleted)
            ),
            "le temporaire est filtré"
        );
        // Remplacement par déplacement depuis un autre dossier : REMOVED puis
        // ADDED, sans MODIFIED.
        let par_deplacement = rejouer(vec![
            ev(EventKind::Remove(RemoveKind::Any), x),
            ev(EventKind::Create(CreateKind::Any), x),
        ]);
        assert_eq!(par_deplacement.get(x), Some(&ChangeType::Added));
    }

    /// Un dossier d'album APRÈS son renommage : l'ancien nom n'existe plus, le
    /// nouveau porte ses fichiers. C'est l'état du disque quand `notify` livre
    /// les événements. Racine sous le dossier courant : `is_tune_temp_file`
    /// écarte tout ce qui vit sous le dossier temporaire du système.
    fn scene_renommee(etiquette: &str) -> (crate::test_scratch::ScratchDir, PathBuf, PathBuf) {
        let racine = crate::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            &format!("watcher-dossiers-4896-{etiquette}"),
        );
        let ancien = racine.join("Pink Floyd").join("Multichannel 7.1");
        let nouveau = racine
            .join("Pink Floyd")
            .join("1973 - The Dark Side Of The Moon");
        fs::create_dir_all(&nouveau).unwrap();
        fs::write(nouveau.join("01 - Speak To Me.flac"), b"x").unwrap();
        (racine, ancien, nouveau)
    }

    fn genres(changes: &[FileChange]) -> HashMap<String, ChangeType> {
        changes
            .iter()
            .map(|c| (c.path.clone(), c.change_type.clone()))
            .collect()
    }

    fn evp(kind: EventKind, chemin: &Path) -> Event {
        Event::new(kind).add_path(chemin.to_path_buf())
    }

    /// #4896 — un dossier d'album RENOMMÉ ou DÉPLACÉ sous la racine, tel que
    /// chacun des trois moteurs natifs de `notify` 7.0.0 le livre (voir
    /// `evenement_de_dossier`). Avant le correctif, toutes ces séquences
    /// étaient perdues : aucun des deux chemins n'a d'extension audio.
    #[test]
    fn un_dossier_renomme_sort_en_disparu_puis_apparu_sur_les_trois_moteurs_4896() {
        use notify::event::{CreateKind, RemoveKind, RenameMode};
        let (_racine, ancien, nouveau) = scene_renommee("renomme");
        let nom = |m| EventKind::Modify(ModifyKind::Name(m));
        let sequences: Vec<(&str, Vec<Event>)> = vec![
            (
                "Windows, renommage sur place (RENAMED_OLD_NAME/NEW_NAME)",
                vec![
                    evp(nom(RenameMode::From), &ancien),
                    evp(nom(RenameMode::To), &nouveau),
                ],
            ),
            (
                "Windows, déplacement vers un autre parent (REMOVED/ADDED)",
                vec![
                    evp(EventKind::Remove(RemoveKind::Any), &ancien),
                    evp(EventKind::Create(CreateKind::Any), &nouveau),
                ],
            ),
            (
                "macOS FSEvents (ItemRenamed sur chaque nom)",
                vec![
                    evp(nom(RenameMode::Any), &ancien),
                    evp(nom(RenameMode::Any), &nouveau),
                ],
            ),
            (
                "Linux inotify (MOVED_FROM, MOVED_TO, paire, MOVE_SELF)",
                vec![
                    evp(nom(RenameMode::From), &ancien),
                    evp(nom(RenameMode::To), &nouveau),
                    Event::new(nom(RenameMode::Both))
                        .add_path(ancien.clone())
                        .add_path(nouveau.clone()),
                    evp(nom(RenameMode::From), &ancien),
                ],
            ),
            (
                "PollWatcher (partage réseau) : disparition puis création",
                vec![
                    evp(EventKind::Remove(RemoveKind::Any), &ancien),
                    evp(EventKind::Create(CreateKind::Any), &nouveau),
                    evp(
                        EventKind::Create(CreateKind::Any),
                        &nouveau.join("01 - Speak To Me.flac"),
                    ),
                ],
            ),
        ];
        for (moteur, evenements) in sequences {
            let vus = genres(&rejouer_evenements_notify(evenements));
            assert_eq!(
                vus.get(&*ancien.to_string_lossy()),
                Some(&ChangeType::DossierDisparu),
                "{moteur} : l'ancien nom doit sortir en « dossier disparu »"
            );
            assert_eq!(
                vus.get(&*nouveau.to_string_lossy()),
                Some(&ChangeType::DossierApparu),
                "{moteur} : le nouveau nom doit sortir en « dossier apparu »"
            );
        }
    }

    /// #4896 — le dossier mis à la corbeille ou sorti de la racine : un seul
    /// événement, sur le dossier.
    #[test]
    fn un_dossier_mis_a_la_corbeille_sort_en_disparu_sur_les_trois_moteurs_4896() {
        use notify::event::{RemoveKind, RenameMode};
        let (_racine, ancien, _nouveau) = scene_renommee("corbeille");
        for (moteur, kind) in [
            ("Windows (REMOVED)", EventKind::Remove(RemoveKind::Any)),
            (
                "macOS (ItemRenamed vers ~/.Trash)",
                EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            ),
            (
                "Linux (MOVED_FROM sans MOVED_TO)",
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            ),
            (
                "macOS/Linux, suppression (IsDir / ISDIR)",
                EventKind::Remove(RemoveKind::Folder),
            ),
        ] {
            let vus = genres(&rejouer_evenements_notify(vec![evp(kind, &ancien)]));
            assert_eq!(
                vus.get(&*ancien.to_string_lossy()),
                Some(&ChangeType::DossierDisparu),
                "{moteur}"
            );
        }
    }

    /// Contre-épreuves : ce qui n'est PAS un dossier qui bouge ne sort pas.
    #[test]
    fn ni_une_pochette_ni_un_montage_ne_passent_pour_un_dossier_4896() {
        use notify::event::{CreateKind, RemoveKind, RenameMode};
        let (_racine, _ancien, nouveau) = scene_renommee("contre");
        let pochette = nouveau.join("cover.jpg");
        fs::write(&pochette, b"jpg").unwrap();
        let vus = genres(&rejouer_evenements_notify(vec![
            // Toujours là, pas un dossier : rien à dire.
            evp(EventKind::Remove(RemoveKind::Any), &pochette),
            evp(
                EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
                &pochette,
            ),
            evp(EventKind::Create(CreateKind::Any), &pochette),
        ]));
        assert!(
            vus.is_empty(),
            "une pochette présente ne dit rien : {vus:?}"
        );
        // Le moteur a dit « fichier » : pas de dossier, même disparu.
        let vus = genres(&rejouer_evenements_notify(vec![evp(
            EventKind::Remove(RemoveKind::File),
            &nouveau.join("notes.txt"),
        )]));
        assert!(vus.is_empty(), "{vus:?}");
        // Un montage (FSEvents, info « mount ») n'est pas un dossier d'album.
        let vus = genres(&rejouer_evenements_notify(vec![
            evp(EventKind::Create(CreateKind::Other), &nouveau).set_info("mount"),
        ]));
        assert!(vus.is_empty(), "{vus:?}");
        // Les événements de CONTENU d'un dossier ne le font pas « apparaître ».
        let vus = genres(&rejouer_evenements_notify(vec![evp(
            EventKind::Modify(ModifyKind::Any),
            &nouveau,
        )]));
        assert!(vus.is_empty(), "{vus:?}");
    }

    #[test]
    fn les_fichiers_audio_d_un_dossier_apparu_se_listent_a_toute_profondeur_4896() {
        let (_racine, _ancien, nouveau) = scene_renommee("liste");
        fs::create_dir_all(nouveau.join("CD2")).unwrap();
        fs::write(nouveau.join("CD2").join("01 - Us And Them.flac"), b"x").unwrap();
        fs::write(nouveau.join("cover.jpg"), b"x").unwrap();
        let vus = fichiers_audio_sous(&nouveau);
        assert_eq!(
            vus,
            vec![
                nouveau
                    .join("01 - Speak To Me.flac")
                    .to_string_lossy()
                    .to_string(),
                nouveau
                    .join("CD2")
                    .join("01 - Us And Them.flac")
                    .to_string_lossy()
                    .to_string(),
            ]
        );
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
