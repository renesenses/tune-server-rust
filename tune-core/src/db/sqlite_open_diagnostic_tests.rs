use super::*;
use crate::db::sqlite::SqliteDb;

fn failure(path: &Path) -> String {
    match SqliteDb::open(path.to_str().unwrap()) {
        Ok(_) => panic!("fixture must fail to open"),
        Err(error) => error,
    }
}

#[test]
fn missing_parent_names_observation_without_creating_it() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("missing");
    let path = parent.join("tune.db");
    let error = failure(&path);
    assert!(error.contains("sqlite open"), "{error}");
    assert!(error.contains("unable to open database file"), "{error}");
    assert!(
        error.contains("cannot inspect parent directory"),
        "missing parent must be identified: {error}"
    );
    assert!(error.contains(parent.to_str().unwrap()), "{error}");
    assert!(
        !parent.exists(),
        "diagnostic must not create a new data location"
    );
}

#[test]
fn directory_as_database_is_identified_without_modifying_contents() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tune.db");
    std::fs::create_dir(&path).unwrap();
    let sentinel = path.join("keep");
    std::fs::write(&sentinel, b"preserve").unwrap();
    let error = failure(&path);
    assert!(
        error.contains("database path is a directory"),
        "directory must not be diagnosed as a permission error: {error}"
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
}

#[test]
fn regular_file_as_parent_is_identified() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("not-a-directory");
    std::fs::write(&parent, b"preserve").unwrap();
    let error = failure(&parent.join("tune.db"));
    assert!(error.contains("parent path is not a directory"), "{error}");
    assert_eq!(std::fs::read(parent).unwrap(), b"preserve");
}

#[cfg(target_os = "linux")]
#[test]
fn nonwritable_parent_reports_real_access_error_and_preserves_mode() {
    use std::os::unix::fs::PermissionsExt;
    // Root bypasses DAC. This witness must run under an ordinary account.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "permission witness requires non-root runner"
    );
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("protected");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    let error = failure(&parent.join("tune.db"));
    let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
    // Restore fixture ownership permissions before assertions / TempDir cleanup.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode, 0o500, "diagnostic must not change permissions");
    assert!(
        error.contains("parent write/search access check failed"),
        "{error}"
    );
    assert!(
        error.contains("Permission denied"),
        "preserve OS reason: {error}"
    );
    assert!(!parent.join("tune.db").exists());
}

#[test]
fn ordinary_writable_database_and_memory_still_open() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tune.db");
    let db = SqliteDb::open(path.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    assert!(path.is_file());
    SqliteDb::open(":memory:").unwrap();
}

#[test]
fn uri_error_is_preserved_without_literal_filesystem_probe() {
    let original = rusqlite::Error::InvalidPath("file:ignored".into());
    let error = describe("file:ignored?mode=ro", &original);
    assert_eq!(
        error,
        format!("sqlite open file:ignored?mode=ro: {original}")
    );
}
