//! Shared track-import helpers for the library scanners.
//!
//! Both the manual scan ([`crate::routes::system::scan`]) and the auto/startup +
//! watcher scans ([`crate::auto_scan`]) turn a [`ScannedFile`]'s
//! [`TrackMetadata`] into a DB [`Track`] row. This module holds the single
//! field-mapping they share so the three former copies cannot drift again — they
//! had already diverged: the manual *insert* path omitted `disc_subtitle`, and
//! the auto/watcher helper omitted `genres` and `composer`.
//!
//! Artist/album *resolution* still lives with each caller for now (it needs
//! batch-wide compilation context); this module owns only the per-file field
//! mapping, which every scan path shares verbatim.

use tune_core::db::models::Track;
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;

/// Serialize the parsed multi-genre list to a JSON array string for
/// `tracks.genres`. Falls back to splitting the single `genre` tag for legacy
/// rows that predate multi-genre parsing.
pub fn build_genres_json(genres: &[String], genre: Option<&str>) -> Option<String> {
    if !genres.is_empty() {
        Some(serde_json::to_string(genres).unwrap_or_default())
    } else if let Some(g) = genre.filter(|g| !g.is_empty()) {
        // Split in case the single tag carries separators (legacy data).
        let split = tune_core::metadata::split_genre_tag(g);
        if split.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&split).unwrap_or_default())
        }
    } else {
        None
    }
}

/// Map a [`ScannedFile`]'s metadata onto a DB [`Track`] row.
///
/// `album_id` / `artist_id` / `track_artist_name` come from the caller's
/// artist/album resolution. The title falls back to the file stem when the tag
/// has none. `id` is left `None`; the update path sets it afterwards.
pub fn build_track_row(
    meta: &TrackMetadata,
    sf: &ScannedFile,
    album_id: Option<i64>,
    artist_id: Option<i64>,
    track_artist_name: &str,
) -> Track {
    let title = meta.title.clone().unwrap_or_else(|| {
        std::path::Path::new(&sf.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let mut track = Track::new(title);
    track.album_id = album_id;
    track.artist_id = artist_id;
    track.artist_name = Some(track_artist_name.to_string());
    track.album_artist = meta.album_artist.clone();
    track.album_title = meta.album.clone();
    track.disc_number = meta.disc_number.unwrap_or(1) as i32;
    track.disc_subtitle = meta.disc_subtitle.clone();
    track.track_number = meta.track_number.unwrap_or(0) as i32;
    track.duration_ms = meta.duration_ms.unwrap_or(0) as i64;
    track.file_path = Some(sf.path.clone());
    track.format = meta.format.clone();
    track.sample_rate = meta.sample_rate.map(|s| s as i32);
    track.bit_depth = meta.bit_depth.map(|b| b as i32);
    track.channels = meta.channels.unwrap_or(2) as i32;
    track.file_size = Some(sf.file_size as i64);
    track.file_mtime = Some(sf.mtime as f64);
    track.audio_hash = sf.audio_hash.clone();
    track.genre = meta.genre.clone();
    track.genres = build_genres_json(&meta.genres, meta.genre.as_deref());
    track.composer = meta
        .credits
        .iter()
        .find(|c| c.role == "composer")
        .map(|c| c.name.clone());
    track.year = meta.year.map(|y| y as i32);
    track.bpm = meta.bpm;
    track.label = meta.label.clone();
    track.isrc = meta.isrc.clone();
    track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
    track.comments = meta.comment.clone();
    track
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::metadata::{TrackCredit, TrackMetadata};
    use tune_core::scanner::walker::ScannedFile;

    fn sf(path: &str) -> ScannedFile {
        ScannedFile {
            path: path.to_string(),
            metadata: None,
            audio_hash: Some("hash-1".into()),
            file_size: 4096,
            mtime: 1_700_000_000,
        }
    }

    #[test]
    fn build_genres_json_prefers_parsed_list() {
        let g = build_genres_json(&["Jazz".into(), "Fusion".into()], Some("ignored"));
        assert_eq!(g.as_deref(), Some(r#"["Jazz","Fusion"]"#));
    }

    #[test]
    fn build_genres_json_falls_back_to_single_tag_split() {
        // Empty parsed list → split the legacy single tag.
        let g = build_genres_json(&[], Some("Jazz; Fusion"));
        assert_eq!(g.as_deref(), Some(r#"["Jazz","Fusion"]"#));
        // Nothing at all → None (not an empty-array string).
        assert_eq!(build_genres_json(&[], None), None);
        assert_eq!(build_genres_json(&[], Some("")), None);
    }

    #[test]
    fn build_track_row_maps_every_field_incl_previously_dropped_ones() {
        let meta = TrackMetadata {
            title: Some("So What".into()),
            album: Some("Kind of Blue".into()),
            album_artist: Some("Miles Davis".into()),
            disc_number: Some(1),
            disc_subtitle: Some("Side A".into()),
            track_number: Some(1),
            duration_ms: Some(544_000),
            sample_rate: Some(44_100),
            bit_depth: Some(24),
            channels: Some(2),
            format: Some("flac".into()),
            year: Some(1959),
            bpm: Some(136.0),
            label: Some("Columbia".into()),
            isrc: Some("USSM15900001".into()),
            musicbrainz_recording_id: Some("rec-1".into()),
            comment: Some("remaster".into()),
            genres: vec!["Jazz".into(), "Modal".into()],
            genre: Some("Jazz".into()),
            credits: vec![TrackCredit {
                name: "Miles Davis".into(),
                role: "composer".into(),
                instrument: None,
            }],
            ..Default::default()
        };
        let track = build_track_row(&meta, &sf("/m/kob/01.flac"), Some(7), Some(3), "Miles Davis");

        assert_eq!(track.id, None);
        assert_eq!(track.title, "So What");
        assert_eq!(track.album_id, Some(7));
        assert_eq!(track.artist_id, Some(3));
        assert_eq!(track.artist_name.as_deref(), Some("Miles Davis"));
        assert_eq!(track.album_title.as_deref(), Some("Kind of Blue"));
        // disc_subtitle was dropped by the old manual *insert* path.
        assert_eq!(track.disc_subtitle.as_deref(), Some("Side A"));
        assert_eq!(track.duration_ms, 544_000);
        assert_eq!(track.sample_rate, Some(44_100));
        assert_eq!(track.bit_depth, Some(24));
        assert_eq!(track.channels, 2);
        assert_eq!(track.file_path.as_deref(), Some("/m/kob/01.flac"));
        assert_eq!(track.file_size, Some(4096));
        assert_eq!(track.audio_hash.as_deref(), Some("hash-1"));
        // genres + composer were dropped by the old auto/watcher helper.
        assert_eq!(track.genres.as_deref(), Some(r#"["Jazz","Modal"]"#));
        assert_eq!(track.composer.as_deref(), Some("Miles Davis"));
        assert_eq!(track.year, Some(1959));
        assert_eq!(track.bpm, Some(136.0));
        assert_eq!(track.isrc.as_deref(), Some("USSM15900001"));
        assert_eq!(track.comments.as_deref(), Some("remaster"));
    }

    #[test]
    fn build_track_row_title_falls_back_to_file_stem_and_defaults() {
        let meta = TrackMetadata::default();
        let track = build_track_row(&meta, &sf("/m/x/Untitled Take.flac"), None, None, "Unknown Artist");
        assert_eq!(track.title, "Untitled Take");
        // Sensible defaults when tags are absent.
        assert_eq!(track.disc_number, 1);
        assert_eq!(track.track_number, 0);
        assert_eq!(track.channels, 2);
        assert_eq!(track.duration_ms, 0);
        assert_eq!(track.genres, None);
        assert_eq!(track.composer, None);
    }
}
