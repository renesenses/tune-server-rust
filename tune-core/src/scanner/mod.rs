pub mod hasher;
pub mod quality;
pub mod walker;
pub mod watcher;

/// Tune's own streaming/prefetch temp files (written to the system temp dir
/// during transcodes) must never be indexed as library tracks, even when the
/// configured music folder is a parent of the temp dir (e.g. the whole user
/// profile). Matched by name so the guard also works if the temp dir moves.
pub fn is_tune_temp_file(path: &std::path::Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if name.starts_with("tune-stream-") || name.starts_with("tune-prefetch-") {
        return true;
    }
    path.starts_with(std::env::temp_dir())
}

#[cfg(test)]
mod tune_temp_file_tests {
    use super::is_tune_temp_file;
    use std::path::Path;

    #[test]
    fn matches_stream_and_prefetch_names_anywhere() {
        // Windows separators only parse as separators on Windows.
        #[cfg(windows)]
        assert!(is_tune_temp_file(Path::new(
            "C:\\Users\\U\\AppData\\Local\\Temp\\tune-stream-abc.flac"
        )));
        assert!(is_tune_temp_file(Path::new("/music/tune-stream-abc.flac")));
        assert!(is_tune_temp_file(Path::new(
            "/music/tune-prefetch-xyz.flac"
        )));
    }

    #[test]
    fn matches_any_file_inside_system_temp() {
        let p = std::env::temp_dir().join("whatever.flac");
        assert!(is_tune_temp_file(&p));
    }

    #[test]
    fn keeps_normal_library_files() {
        assert!(!is_tune_temp_file(Path::new("/music/Albums/track01.flac")));
        assert!(!is_tune_temp_file(Path::new(
            "/music/tune - the band/song.flac"
        )));
    }
}
