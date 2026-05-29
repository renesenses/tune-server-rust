use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::{info, warn};

const MAX_BACKUPS: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub filename: String,
    pub size: u64,
    pub created_at: String,
}

pub fn create_backup(db_path: &str) -> Option<BackupInfo> {
    let db_file = Path::new(db_path);
    if !db_file.exists() {
        return None;
    }

    let backup_dir = db_file.parent()?.join("backups");
    fs::create_dir_all(&backup_dir).ok()?;

    let stem = db_file.file_stem()?.to_str()?;
    let ext = db_file.extension().map(|e| e.to_str().unwrap_or("db")).unwrap_or("db");
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{stem}_{timestamp}.{ext}");
    let backup_path = backup_dir.join(&backup_name);

    if let Err(e) = fs::copy(db_file, &backup_path) {
        warn!(error = %e, "database_backup_error");
        return None;
    }

    for suffix in ["-wal", "-shm"] {
        let wal = db_file.with_file_name(format!("{}{suffix}", db_file.file_name()?.to_str()?));
        if wal.exists() {
            let dest = backup_dir.join(format!("{backup_name}{suffix}"));
            let _ = fs::copy(&wal, &dest);
        }
    }

    info!(path = %backup_path.display(), "database_backup_created");

    prune_backups(&backup_dir, stem, ext);

    let meta = fs::metadata(&backup_path).ok()?;
    let created = meta
        .modified()
        .ok()
        .and_then(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            Some(dt.to_rfc3339())
        })
        .unwrap_or_default();

    Some(BackupInfo {
        filename: backup_name,
        size: meta.len(),
        created_at: created,
    })
}

pub fn list_backups(db_path: &str) -> Vec<BackupInfo> {
    let db_file = Path::new(db_path);
    let backup_dir = match db_file.parent() {
        Some(p) => p.join("backups"),
        None => return vec![],
    };
    if !backup_dir.exists() {
        return vec![];
    }

    let stem = db_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tune_server");
    let ext = db_file
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("db");

    let pattern = format!("{stem}_");
    let suffix = format!(".{ext}");

    let mut backups: Vec<(PathBuf, BackupInfo)> = fs::read_dir(&backup_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with(&pattern) && name.ends_with(&suffix) {
                let meta = entry.metadata().ok()?;
                let created = meta
                    .modified()
                    .ok()
                    .and_then(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        Some(dt.to_rfc3339())
                    })
                    .unwrap_or_default();
                Some((
                    entry.path(),
                    BackupInfo {
                        filename: name,
                        size: meta.len(),
                        created_at: created,
                    },
                ))
            } else {
                None
            }
        })
        .collect();

    backups.sort_by(|a, b| b.1.filename.cmp(&a.1.filename));
    backups.into_iter().map(|(_, info)| info).collect()
}

pub fn restore_backup(db_path: &str, filename: &str) -> bool {
    let db_file = Path::new(db_path);
    let backup_dir = match db_file.parent() {
        Some(p) => p.join("backups"),
        None => return false,
    };
    let backup_path = backup_dir.join(filename);

    if !backup_path.exists() {
        return false;
    }

    if let Ok(resolved) = backup_path.canonicalize() {
        if let Ok(dir_resolved) = backup_dir.canonicalize() {
            if !resolved.starts_with(&dir_resolved) {
                warn!("path_traversal_blocked");
                return false;
            }
        }
    }

    for suffix in ["-wal", "-shm"] {
        let wal = db_file.with_file_name(format!(
            "{}{}",
            db_file.file_name().unwrap_or_default().to_str().unwrap_or(""),
            suffix
        ));
        if wal.exists() {
            let _ = fs::remove_file(&wal);
        }
    }

    match fs::copy(&backup_path, db_file) {
        Ok(_) => {
            info!(backup = filename, "database_restored");
            true
        }
        Err(e) => {
            warn!(error = %e, backup = filename, "database_restore_error");
            false
        }
    }
}

fn prune_backups(backup_dir: &Path, stem: &str, ext: &str) {
    let pattern = format!("{stem}_");
    let suffix = format!(".{ext}");

    let mut files: Vec<PathBuf> = fs::read_dir(backup_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with(&pattern) && name.ends_with(&suffix) {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();

    files.sort();
    while files.len() > MAX_BACKUPS {
        if let Some(old) = files.first() {
            let _ = fs::remove_file(old);
            for s in ["-wal", "-shm"] {
                let wal = old.with_file_name(format!("{}{s}", old.file_name().unwrap_or_default().to_str().unwrap_or("")));
                let _ = fs::remove_file(&wal);
            }
            info!(path = %old.display(), "database_backup_pruned");
        }
        files.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_backups_empty_dir() {
        let backups = list_backups("/nonexistent/path/tune.db");
        assert!(backups.is_empty());
    }

    #[test]
    fn restore_nonexistent() {
        assert!(!restore_backup("/tmp/test.db", "nonexistent_backup.db"));
    }

    #[test]
    fn create_and_list_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        fs::write(&db_path, b"test data").unwrap();

        let info = create_backup(db_path.to_str().unwrap());
        assert!(info.is_some());
        let info = info.unwrap();
        assert!(info.filename.starts_with("test_"));
        assert!(info.size > 0);

        let list = list_backups(db_path.to_str().unwrap());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, info.filename);
    }

    #[test]
    fn create_and_restore_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        fs::write(&db_path, b"original").unwrap();

        let info = create_backup(db_path.to_str().unwrap()).unwrap();

        fs::write(&db_path, b"modified").unwrap();
        assert_eq!(fs::read_to_string(&db_path).unwrap(), "modified");

        assert!(restore_backup(db_path.to_str().unwrap(), &info.filename));
        assert_eq!(fs::read_to_string(&db_path).unwrap(), "original");
    }

    #[test]
    fn prune_keeps_max() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        for i in 0..8 {
            fs::write(&db_path, format!("data{i}")).unwrap();
            create_backup(db_path.to_str().unwrap());
        }

        let list = list_backups(db_path.to_str().unwrap());
        assert!(list.len() <= MAX_BACKUPS);
    }
}
