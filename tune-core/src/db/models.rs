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
            Some("DSD".into())
        } else if sr > 44100 || bd > 16 {
            Some("Hi-Res".into())
        } else if fmt == "flac" || fmt == "alac" || fmt == "wav" || fmt == "aiff" || fmt == "wv" {
            Some("CD".into())
        } else if fmt == "mp3" || fmt == "ogg" || fmt == "opus" || fmt == "wma" || fmt == "m4a" {
            Some("Lossy".into())
        } else if !fmt.is_empty() {
            Some("CD".into())
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
    pub composer: Option<String>,
    pub year: Option<i32>,
    pub bpm: Option<f64>,
    pub label: Option<String>,
    pub musicbrainz_recording_id: Option<String>,
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
            composer: None,
            year: None,
            bpm: None,
            label: None,
            musicbrainz_recording_id: None,
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
