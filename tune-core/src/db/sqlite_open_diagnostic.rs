//! Best-effort observations after SQLite failed to open a file.
//!
//! Never creates a directory/file, changes permissions, or retries SQLite.
//! Filesystem state may change after the failure; observations are not a
//! diagnosis of the original incident. The original SQLite error stays first.
use std::path::Path;

pub(super) fn describe(path: &str, error: &rusqlite::Error) -> String {
    let mut message = format!("sqlite open {path}: {error}");
    // SQLite URI filenames have their own path/query semantics. Do not probe
    // a literal "file:..." pathname or turn an URI mode into a permissions hint.
    if path.starts_with("file:") || path == ":memory:" || path.is_empty() {
        return message;
    }
    let resolved = std::path::absolute(path).unwrap_or_else(|_| path.into());
    let parent = resolved.parent().unwrap_or_else(|| Path::new("."));
    message.push_str(&format!("; absolute path: {}", resolved.display()));
    let observation = match std::fs::metadata(parent) {
        Err(e) => format!("cannot inspect parent directory {}: {e}", parent.display()),
        Ok(metadata) if !metadata.is_dir() => {
            format!("parent path is not a directory: {}", parent.display())
        }
        Ok(_) => match std::fs::metadata(&resolved) {
            Ok(metadata) if metadata.is_dir() => {
                format!("database path is a directory: {}", resolved.display())
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                format!("cannot inspect database path: {e}")
            }
            _ => access_observation(parent, &resolved),
        },
    };
    message.push_str(&format!(
        "; filesystem observation: {observation}; check TUNE_DB_PATH and its mount: \
         the database must be a file and its parent directory must exist and be \
         writable/searchable by the server account (including SQLite journal/WAL files). \
         For Docker, inspect the container UID/GID and /data mount access; \
         do not delete the database or run the server as root to bypass this error."
    ));
    message
}

#[cfg(target_os = "linux")]
fn access_observation(parent: &Path, database: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    // faccessat with AT_EACCESS checks the effective credentials and kernel
    // access rules, rather than guessing from mode bits (ACLs, mounts, etc.).
    fn check(path: &Path, mode: libc::c_int) -> Result<(), std::io::Error> {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: a live NUL-terminated pathname is supplied, with no writes
        // through the pointer. AT_FDCWD and AT_EACCESS are valid Linux flags.
        let result =
            unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    if let Err(e) = check(parent, libc::W_OK | libc::X_OK) {
        return format!("parent write/search access check failed: {e}");
    }
    // A missing database can be created by SQLite; only existing files need
    // their own read/write access check. This check never opens the file.
    if database.exists()
        && let Err(e) = check(database, libc::R_OK | libc::W_OK)
    {
        return format!("database read/write access check failed: {e}");
    }
    "no path/access obstruction observed after the SQLite failure; cause not determined".into()
}

#[cfg(not(target_os = "linux"))]
fn access_observation(_parent: &Path, _database: &Path) -> String {
    "no directory/type obstruction observed; effective write access not checked on this platform"
        .into()
}

#[cfg(test)]
#[path = "sqlite_open_diagnostic_tests.rs"]
mod tests;
