pub mod activite;
pub mod album_folder;
pub mod compilation;
pub mod cue;
pub mod cue_album;
pub mod cue_bibliotheque;
pub mod hasher;
pub mod obstacle;
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
    // La copie de travail de « Écrire dans les fichiers » (édition d'album,
    // tranche 4) : posée à CÔTÉ du fichier, le temps d'y écrire les balises,
    // puis renommée sur lui. Le surveillant ne doit jamais en faire une piste.
    if name.starts_with(crate::metadata::tag_writer::PREFIXE_COPIE_DE_TRAVAIL) {
        return true;
    }
    // tmp-autorise: comparaison seule : on LIT la racine pour reconnaître nos propres temporaires.
    path.starts_with(std::env::temp_dir())
}

#[cfg(test)]
mod tune_temp_file_tests {
    use super::is_tune_temp_file;
    use std::path::Path;

    #[test]
    fn matches_stream_and_prefetch_names_anywhere() {
        assert!(is_tune_temp_file(Path::new(
            "/music/tune-prefetch-xyz.flac"
        )));
        assert!(is_tune_temp_file(Path::new("/music/tune-stream-abc.flac")));
        assert!(is_tune_temp_file(Path::new(
            "/anywhere/else/tune-stream-abc.flac"
        )));
    }

    /// La copie de travail d'« Écrire dans les fichiers » vit dans le dossier
    /// de l'album, sous une extension audio : seul son nom l'écarte.
    #[test]
    fn ecarte_la_copie_de_travail_des_balises() {
        assert!(is_tune_temp_file(Path::new(
            "/music/Album/tune-balises-4242-0-17.flac"
        )));
        assert!(!is_tune_temp_file(Path::new(
            "/music/Album/tune-balises.flac"
        )));
    }

    /// A backslash-separated path only parses as a path on Windows. On Unix
    /// `Path::file_name` does not split on `\`, so it hands back the whole
    /// string and the `tune-stream-` prefix check never sees the leaf. The
    /// function is right — that spelling only ever occurs on Windows, where
    /// `file_name` does split — so it is the assertion that has to be gated.
    #[cfg(windows)]
    #[test]
    fn matches_a_windows_temp_path() {
        assert!(is_tune_temp_file(Path::new(
            "C:\\Users\\U\\AppData\\Local\\Temp\\tune-stream-abc.flac"
        )));
    }

    #[test]
    fn matches_any_file_inside_system_temp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("whatever.flac");
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
