use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    pub id: Option<i64>,
    pub name: String,
    pub sort_name: Option<String>,
    pub musicbrainz_id: Option<String>,
    pub discogs_id: Option<String>,
    pub bio: Option<String>,
    pub image_path: Option<String>,
    pub image_source: Option<String>,
}

impl Artist {
    pub fn new(name: String) -> Self {
        Self {
            id: None,
            name,
            sort_name: None,
            musicbrainz_id: None,
            discogs_id: None,
            bio: None,
            image_path: None,
            image_source: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: Option<i64>,
    pub title: String,
    pub artist_id: Option<i64>,
    pub artist_name: Option<String>,
    pub year: Option<i32>,
    pub original_year: Option<i32>,
    pub genre: Option<String>,
    /// JSON array of all genres (e.g. `["Jazz","Fusion","Progressive"]`)
    #[serde(default)]
    pub genres: Option<String>,
    pub disc_count: Option<i32>,
    pub track_count: Option<i32>,
    pub cover_path: Option<String>,
    pub source: String,
    pub source_id: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub barcode: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub bio: Option<String>,
    pub musicbrainz_release_id: Option<String>,
    pub musicbrainz_release_group_id: Option<String>,
    pub release_date: Option<String>,
    pub original_date: Option<String>,
}

impl Album {
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("quality".into(), serde_json::json!(self.quality()));
        }
        v
    }

    pub fn quality(&self) -> Option<String> {
        let fmt = self.format.as_deref().unwrap_or("");
        let sr = self.sample_rate.unwrap_or(0);
        let bd = self.bit_depth.unwrap_or(0);
        if fmt.contains("dsf") || fmt.contains("dff") || fmt.contains("dsd") {
            Some("dsd".into())
        } else if sr > 48000 || bd > 16 {
            Some("hi-res".into())
        } else if fmt == "mp3" || fmt == "ogg" || fmt == "opus" || fmt == "wma" || fmt == "aac" {
            Some("lossy".into())
        } else if !fmt.is_empty() {
            Some("cd".into())
        } else {
            None
        }
    }

    pub fn new(title: String) -> Self {
        Self {
            id: None,
            title,
            artist_id: None,
            artist_name: None,
            year: None,
            original_year: None,
            genre: None,
            genres: None,
            disc_count: None,
            track_count: None,
            cover_path: None,
            source: "local".to_string(),
            source_id: None,
            label: None,
            catalog_number: None,
            barcode: None,
            format: None,
            sample_rate: None,
            bit_depth: None,
            bio: None,
            musicbrainz_release_id: None,
            musicbrainz_release_group_id: None,
            release_date: None,
            original_date: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: Option<i64>,
    pub title: String,
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    pub artist_id: Option<i64>,
    pub artist_name: Option<String>,
    pub album_artist: Option<String>,
    pub disc_number: i32,
    pub disc_subtitle: Option<String>,
    pub track_number: i32,
    pub duration_ms: i64,
    pub file_path: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub channels: i32,
    pub file_mtime: Option<f64>,
    pub file_size: Option<i64>,
    pub audio_hash: Option<String>,
    pub source: String,
    pub source_id: Option<String>,
    pub isrc: Option<String>,
    pub genre: Option<String>,
    /// JSON array of all genres (e.g. `["Jazz","Fusion","Progressive"]`)
    #[serde(default)]
    pub genres: Option<String>,
    pub composer: Option<String>,
    pub year: Option<i32>,
    pub bpm: Option<f64>,
    pub label: Option<String>,
    pub musicbrainz_recording_id: Option<String>,
    pub cover_path: Option<String>,
}

impl Track {
    pub fn new(title: String) -> Self {
        Self {
            id: None,
            title,
            album_id: None,
            album_title: None,
            artist_id: None,
            artist_name: None,
            album_artist: None,
            disc_number: 1,
            disc_subtitle: None,
            track_number: 0,
            duration_ms: 0,
            file_path: None,
            format: None,
            sample_rate: None,
            bit_depth: None,
            channels: 2,
            file_mtime: None,
            file_size: None,
            audio_hash: None,
            source: "local".to_string(),
            source_id: None,
            isrc: None,
            genre: None,
            genres: None,
            composer: None,
            year: None,
            bpm: None,
            label: None,
            musicbrainz_recording_id: None,
            cover_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackCredit {
    pub id: Option<i64>,
    pub track_id: i64,
    pub artist_id: Option<i64>,
    pub artist_name: String,
    pub role: String,
    pub instrument: Option<String>,
    pub position: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_new() {
        let artist = Artist::new("Miles Davis".into());
        assert_eq!(artist.name, "Miles Davis");
        assert!(artist.id.is_none());
        assert!(artist.sort_name.is_none());
        assert!(artist.musicbrainz_id.is_none());
    }

    #[test]
    fn artist_serialization() {
        let artist = Artist {
            id: Some(1),
            name: "Miles Davis".into(),
            sort_name: Some("Davis, Miles".into()),
            musicbrainz_id: Some("mbid-123".into()),
            discogs_id: None,
            bio: Some("Jazz trumpeter".into()),
            image_path: None,
            image_source: None,
        };
        let json = serde_json::to_value(&artist).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["name"], "Miles Davis");
        assert_eq!(json["sort_name"], "Davis, Miles");
    }

    #[test]
    fn album_new() {
        let album = Album::new("Kind of Blue".into());
        assert_eq!(album.title, "Kind of Blue");
        assert_eq!(album.source, "local");
        assert!(album.id.is_none());
        assert!(album.year.is_none());
    }

    #[test]
    fn album_quality_dsd() {
        let mut a = Album::new("DSD Album".into());
        a.format = Some("dsf".into());
        assert_eq!(a.quality(), Some("dsd".into()));
    }

    #[test]
    fn album_quality_dff() {
        let mut a = Album::new("DFF Album".into());
        a.format = Some("dff".into());
        assert_eq!(a.quality(), Some("dsd".into()));
    }

    #[test]
    fn album_quality_hires_by_sample_rate() {
        let mut a = Album::new("HR".into());
        a.format = Some("flac".into());
        a.sample_rate = Some(96000);
        assert_eq!(a.quality(), Some("hi-res".into()));
    }

    #[test]
    fn album_quality_hires_by_bit_depth() {
        let mut a = Album::new("HR".into());
        a.format = Some("flac".into());
        a.bit_depth = Some(24);
        assert_eq!(a.quality(), Some("hi-res".into()));
    }

    #[test]
    fn album_quality_lossy_formats() {
        for fmt in ["mp3", "ogg", "opus", "wma", "aac"] {
            let mut a = Album::new("Test".into());
            a.format = Some(fmt.into());
            assert_eq!(
                a.quality(),
                Some("lossy".into()),
                "Expected lossy for {fmt}"
            );
        }
    }

    #[test]
    fn album_quality_cd() {
        let mut a = Album::new("CD".into());
        a.format = Some("flac".into());
        a.sample_rate = Some(44100);
        a.bit_depth = Some(16);
        assert_eq!(a.quality(), Some("cd".into()));
    }

    #[test]
    fn album_quality_none() {
        let a = Album::new("Unknown".into());
        assert_eq!(a.quality(), None);
    }

    #[test]
    fn album_to_json_includes_quality() {
        let mut a = Album::new("Test".into());
        a.format = Some("mp3".into());
        let json = a.to_json();
        assert_eq!(json["quality"], "lossy");
    }

    #[test]
    fn album_serialization() {
        let album = Album {
            id: Some(1),
            title: "Kind of Blue".into(),
            artist_id: Some(42),
            artist_name: Some("Miles Davis".into()),
            year: Some(1959),
            original_year: Some(1959),
            genre: Some("Jazz".into()),
            genres: Some(r#"["Jazz","Modal Jazz"]"#.into()),
            disc_count: Some(1),
            track_count: Some(5),
            cover_path: Some("hash123".into()),
            source: "local".into(),
            source_id: None,
            label: Some("Columbia".into()),
            catalog_number: None,
            barcode: None,
            format: Some("flac".into()),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            bio: None,
            musicbrainz_release_id: None,
            musicbrainz_release_group_id: None,
            release_date: None,
            original_date: None,
        };
        let json = serde_json::to_value(&album).unwrap();
        assert_eq!(json["title"], "Kind of Blue");
        assert_eq!(json["year"], 1959);
        assert_eq!(json["label"], "Columbia");
    }

    #[test]
    fn track_new() {
        let track = Track::new("So What".into());
        assert_eq!(track.title, "So What");
        assert_eq!(track.disc_number, 1);
        assert_eq!(track.track_number, 0);
        assert_eq!(track.duration_ms, 0);
        assert_eq!(track.channels, 2);
        assert_eq!(track.source, "local");
    }

    #[test]
    fn track_serialization() {
        let track = Track {
            id: Some(1),
            title: "So What".into(),
            album_id: Some(10),
            album_title: Some("Kind of Blue".into()),
            artist_id: Some(42),
            artist_name: Some("Miles Davis".into()),
            album_artist: None,
            disc_number: 1,
            disc_subtitle: None,
            track_number: 1,
            duration_ms: 562_000,
            file_path: Some("/music/so_what.flac".into()),
            format: Some("flac".into()),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: 2,
            file_mtime: None,
            file_size: None,
            audio_hash: None,
            source: "local".into(),
            source_id: None,
            isrc: None,
            genre: Some("Jazz".into()),
            genres: None,
            composer: None,
            year: Some(1959),
            bpm: None,
            label: None,
            musicbrainz_recording_id: None,
            cover_path: None,
        };
        let json = serde_json::to_value(&track).unwrap();
        assert_eq!(json["title"], "So What");
        assert_eq!(json["duration_ms"], 562_000);
        assert_eq!(json["disc_number"], 1);
    }

    #[test]
    fn track_credit_struct() {
        let credit = TrackCredit {
            id: Some(1),
            track_id: 100,
            artist_id: Some(42),
            artist_name: "Miles Davis".into(),
            role: "performer".into(),
            instrument: Some("trumpet".into()),
            position: 0,
        };
        let json = serde_json::to_value(&credit).unwrap();
        assert_eq!(json["artist_name"], "Miles Davis");
        assert_eq!(json["role"], "performer");
        assert_eq!(json["instrument"], "trumpet");
    }
}
