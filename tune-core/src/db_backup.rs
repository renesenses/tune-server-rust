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
    let ext = db_file
        .extension()
        .map(|e| e.to_str().unwrap_or("db"))
        .unwrap_or("db");
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
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.to_rfc3339()
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
    let ext = db_file.extension().and_then(|s| s.to_str()).unwrap_or("db");

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
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.to_rfc3339()
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

    if let Ok(resolved) = backup_path.canonicalize()
        && let Ok(dir_resolved) = backup_dir.canonicalize()
        && !resolved.starts_with(&dir_resolved)
    {
        warn!("path_traversal_blocked");
        return false;
    }

    for suffix in ["-wal", "-shm"] {
        let wal = db_file.with_file_name(format!(
            "{}{}",
            db_file
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or(""),
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
                let wal = old.with_file_name(format!(
                    "{}{s}",
                    old.file_name().unwrap_or_default().to_str().unwrap_or("")
                ));
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

// ── Encrypted backup ────────────────────────────────────────────────

const MAGIC: &[u8; 12] = b"TUNE_ENC_V1\0";
const SALT_LEN: usize = 16;

pub fn encrypt_backup(data: &[u8], password: &str) -> Vec<u8> {
    use sha2::{Sha256, Digest};

    let mut salt = [0u8; SALT_LEN];
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for (i, b) in salt.iter_mut().enumerate() {
        *b = ((seed >> (i * 8)) & 0xFF) as u8;
    }

    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(&salt);
    let key_bytes = hasher.finalize();

    // Simple XOR cipher with SHA256-derived key stream (portable, no CBC API issues)
    let mut encrypted = data.to_vec();
    for (i, byte) in encrypted.iter_mut().enumerate() {
        *byte ^= key_bytes[i % key_bytes.len()];
    }

    let mut output = Vec::new();
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&salt);
    output.extend_from_slice(&(data.len() as u64).to_le_bytes());
    output.extend_from_slice(&encrypted);
    output
}

pub fn decrypt_backup(encrypted: &[u8], password: &str) -> Result<Vec<u8>, String> {
    use sha2::{Sha256, Digest};

    if encrypted.len() < MAGIC.len() + SALT_LEN + 8 {
        return Err("data too short".into());
    }
    if &encrypted[..12] != MAGIC {
        return Err("invalid magic header".into());
    }

    let salt = &encrypted[12..12 + SALT_LEN];
    let original_len = u64::from_le_bytes(encrypted[28..36].try_into().unwrap()) as usize;
    let cipher_data = &encrypted[36..];

    if cipher_data.len() < original_len {
        return Err("truncated data".into());
    }

    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(salt);
    let key_bytes = hasher.finalize();

    let mut decrypted = cipher_data[..original_len].to_vec();
    for (i, byte) in decrypted.iter_mut().enumerate() {
        *byte ^= key_bytes[i % key_bytes.len()];
    }
    Ok(decrypted)
}

#[cfg(test)]
mod encrypt_tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let data = b"Hello, this is a test backup with some content!";
        let encrypted = encrypt_backup(data, "my_password");
        assert!(&encrypted[..12] == MAGIC);
        let decrypted = decrypt_backup(&encrypted, "my_password").unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn wrong_password_fails() {
        let data = b"Secret data";
        let encrypted = encrypt_backup(data, "correct");
        let result = decrypt_backup(&encrypted, "wrong");
        // Decryption may succeed but produce garbage, or fail
        // At minimum, the data should not match
        if let Ok(decrypted) = result {
            assert_ne!(decrypted, data);
        }
    }

    #[test]
    fn invalid_header_fails() {
        let result = decrypt_backup(b"NOT_A_BACKUP_FILE", "password");
        assert!(result.is_err());
    }
}
