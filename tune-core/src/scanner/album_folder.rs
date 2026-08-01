//! Which folder an album lives in.
//!
//! A folder is the one thing on disk that says "these files are one release".
//! Tags cannot: an edition mixes sample rates from disc to disc, a box set
//! repeats the same song titles across discs, and two different rips of the
//! same album share title, artist and year.
//!
//! The only complication is multi-disc layouts. `Album/CD1/` and `Album/CD2/`
//! are one release in two folders, so the album folder is the parent of a
//! disc folder — otherwise a two-disc set would index as two albums.

use std::path::Path;

/// The folder that identifies the album a file belongs to.
///
/// Normally the file's own directory. When that directory is a disc folder
/// (`CD1`, `CD 2`, `Disc 03`, `Disque 2`, `Disk1`), its parent — the release
/// folder — is used instead.
///
/// Returns `None` for a path with no parent directory.
pub fn album_folder(file_path: &str) -> Option<String> {
    let parent = Path::new(file_path).parent()?;
    // No name of its own — an empty parent (a bare filename) or a filesystem
    // root. Nothing to promote to, and nothing to test against the disc pattern.
    let Some(name) = parent.file_name() else {
        return Some(parent.to_string_lossy().into_owned());
    };
    if is_disc_folder(&name.to_string_lossy()) {
        // Promote only to a *named* directory: the grandparent of `/CD1/x.flac`
        // is `/`, and mapping every root-level disc folder onto the filesystem
        // root would merge unrelated albums into one.
        if let Some(release) = parent.parent().filter(|p| p.file_name().is_some()) {
            return Some(release.to_string_lossy().into_owned());
        }
    }
    Some(parent.to_string_lossy().into_owned())
}

/// Whether a folder name denotes a disc within a release rather than a release.
///
/// Deliberately narrow: the name must be *only* a disc marker plus its number.
/// A real album called "Disc-Overy" (Tinie Tempah) or "CD Ripping Sessions" is
/// a release, not a disc folder, so anything carrying other words is left alone.
fn is_disc_folder(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    for prefix in ["cd", "disc", "disk", "disque"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let rest = rest.trim_start_matches([' ', '-', '_', '.']);
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_album_folder_is_the_files_own_directory() {
        assert_eq!(
            album_folder("/music/Green Day/American Idiot/01 - American Idiot.flac").as_deref(),
            Some("/music/Green Day/American Idiot")
        );
    }

    #[test]
    fn disc_subfolders_resolve_to_the_release_folder() {
        // The case this exists for: without it, a two-disc set indexes as two
        // albums, one per disc folder.
        for dir in [
            "CD1", "CD 2", "cd02", "Disc 3", "disc-1", "Disk1", "Disque 2",
        ] {
            assert_eq!(
                album_folder(&format!("/music/Artist/Album/{dir}/05 - Track.flac")).as_deref(),
                Some("/music/Artist/Album"),
                "{dir} should resolve to its parent"
            );
        }
    }

    #[test]
    fn a_release_whose_name_merely_starts_like_a_disc_marker_is_kept() {
        for dir in [
            "Disc-Overy",
            "CD Ripping Sessions",
            "Discovery",
            "Disque en vrac",
        ] {
            let path = format!("/music/Artist/{dir}/01 - Track.flac");
            assert_eq!(
                album_folder(&path).as_deref(),
                Some(format!("/music/Artist/{dir}").as_str()),
                "{dir} is a release name, not a disc folder"
            );
        }
    }

    #[test]
    fn a_disc_folder_at_the_root_keeps_itself() {
        // Nothing above it to promote to, so it stays the album folder rather
        // than collapsing every root-level disc folder into one album.
        assert_eq!(
            album_folder("/CD1/01 - Track.flac").as_deref(),
            Some("/CD1")
        );
    }

    #[test]
    fn a_bare_filename_has_no_album_folder() {
        // `Path::parent` of a bare name is `""`, which identifies nothing.
        assert_eq!(album_folder("track.flac").as_deref(), Some(""));
        assert_eq!(album_folder(""), None);
    }

    #[test]
    fn platform_separators() {
        // `Path` splits on the platform's separator. A backslash path is one
        // long filename on Unix — no parent, hence no album folder — and a real
        // path on Windows. Asserted on both so the difference is documented
        // rather than discovered.
        let got = album_folder(r"C:\music\Artist\Album\01 - Track.flac");
        #[cfg(windows)]
        assert_eq!(got.as_deref(), Some(r"C:\music\Artist\Album"));
        #[cfg(not(windows))]
        assert_eq!(
            got.as_deref(),
            Some(""),
            "one long filename: its parent is the empty path, which identifies \
             no folder — the repo then falls back to title+artist identity"
        );
    }
}
