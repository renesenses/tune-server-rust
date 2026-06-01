//! Windows startup check: detect Program Files install and migrate data
//! to %LOCALAPPDATA%\TuneServer.
//!
//! Called once at startup, before DB initialization.

#[cfg(target_os = "windows")]
pub fn check_and_migrate() {
    use std::path::{Path, PathBuf};
    use tracing::{info, warn};

    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "windows_migrate_cannot_resolve_exe_path");
            return;
        }
    };

    let exe_dir = match exe_path.parent() {
        Some(d) => d,
        None => return,
    };

    // Detect if running from a restricted Program Files directory
    let exe_dir_lower = exe_dir.to_string_lossy().to_lowercase();
    let in_program_files =
        exe_dir_lower.contains("program files (x86)") || exe_dir_lower.contains("program files");

    if !in_program_files {
        return;
    }

    // Resolve target data directory
    let data_dir = match std::env::var("LOCALAPPDATA") {
        Ok(appdata) => PathBuf::from(format!("{appdata}\\TuneServer")),
        Err(_) => {
            warn!("windows_migrate_LOCALAPPDATA_not_set");
            return;
        }
    };

    warn!(
        exe_dir = %exe_dir.display(),
        data_dir = %data_dir.display(),
        "running_from_program_files — data will be stored in %LOCALAPPDATA%\\TuneServer"
    );

    // Create data directory if missing
    if !data_dir.exists() {
        match std::fs::create_dir_all(&data_dir) {
            Ok(_) => info!(path = %data_dir.display(), "windows_migrate_data_dir_created"),
            Err(e) => {
                warn!(path = %data_dir.display(), error = %e, "windows_migrate_data_dir_create_failed");
                return;
            }
        }
    }

    // Migrate old tune.db sitting next to the exe (from pre-migration installs)
    let old_db = exe_dir.join("tune.db");
    let new_db = data_dir.join("tune.db");

    if old_db.exists() && !new_db.exists() {
        info!(
            from = %old_db.display(),
            to = %new_db.display(),
            "migrating database to %LOCALAPPDATA%\\TuneServer"
        );
        match std::fs::copy(&old_db, &new_db) {
            Ok(bytes) => info!(bytes, "windows_migrate_db_copied"),
            Err(e) => warn!(
                from = %old_db.display(),
                to = %new_db.display(),
                error = %e,
                "windows_migrate_db_copy_failed"
            ),
        }
    }
}
