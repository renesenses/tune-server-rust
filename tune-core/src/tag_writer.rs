use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TagUpdate {
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub year: Option<i32>,
    pub comment: Option<String>,
    pub isrc: Option<String>,
    pub bpm: Option<i32>,
    pub label: Option<String>,
    pub lyrics: Option<String>,
}

const ID3_MAP: &[(&str, &str)] = &[
    ("title", "TIT2"),
    ("artist_name", "TPE1"),
    ("album_title", "TALB"),
    ("track_number", "TRCK"),
    ("disc_number", "TPOS"),
    ("genre", "TCON"),
    ("composer", "TCOM"),
    ("year", "TDRC"),
    ("comment", "COMM"),
    ("isrc", "TSRC"),
    ("bpm", "TBPM"),
    ("label", "TPUB"),
];

const VORBIS_MAP: &[(&str, &str)] = &[
    ("title", "TITLE"),
    ("artist_name", "ARTIST"),
    ("album_title", "ALBUM"),
    ("track_number", "TRACKNUMBER"),
    ("disc_number", "DISCNUMBER"),
    ("genre", "GENRE"),
    ("composer", "COMPOSER"),
    ("year", "DATE"),
    ("comment", "COMMENT"),
    ("isrc", "ISRC"),
    ("bpm", "BPM"),
    ("label", "LABEL"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagFormat {
    Id3,
    Vorbis,
    Mp4,
    Unknown,
}

pub fn detect_format(file_path: &str) -> TagFormat {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "mp3" => TagFormat::Id3,
        "flac" | "ogg" | "oga" | "opus" => TagFormat::Vorbis,
        "m4a" | "aac" | "alac" | "mp4" => TagFormat::Mp4,
        "aiff" | "aif" => TagFormat::Id3,
        _ => TagFormat::Unknown,
    }
}

pub async fn write_tags(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let format = detect_format(file_path);
    if format == TagFormat::Unknown {
        return Err("unsupported tag format".into());
    }

    if !Path::new(file_path).exists() {
        return Err("file not found".into());
    }

    let metadata = build_ffmpeg_metadata(update, format);
    if metadata.is_empty() {
        return Ok(WriteResult {
            file_path: file_path.into(),
            fields_written: 0,
        });
    }

    let fields_count = metadata.len();

    let mut args = vec![
        "-i".to_string(),
        file_path.to_string(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
    ];

    for (key, value) in &metadata {
        args.push("-metadata".into());
        args.push(format!("{key}={value}"));
    }

    let temp_path = format!("{file_path}.tmp");
    args.push("-y".into());
    args.push(temp_path.clone());

    let output = tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("ffmpeg: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(format!("ffmpeg tag write failed: {stderr}"));
    }

    tokio::fs::rename(&temp_path, file_path)
        .await
        .map_err(|e| format!("rename: {e}"))?;

    info!(file = file_path, fields = fields_count, "tags_written");

    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: fields_count,
    })
}

fn build_ffmpeg_metadata(update: &TagUpdate, format: TagFormat) -> Vec<(String, String)> {
    let mut metadata = Vec::new();
    let map: &[(&str, &str)] = match format {
        TagFormat::Vorbis => VORBIS_MAP,
        _ => ID3_MAP,
    };

    let fields = tag_update_to_map(update);

    for (field_name, tag_name) in map {
        if let Some(value) = fields.get(*field_name) {
            metadata.push((tag_name.to_string(), value.clone()));
        }
    }

    metadata
}

fn tag_update_to_map(update: &TagUpdate) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(ref v) = update.title {
        map.insert("title".into(), v.clone());
    }
    if let Some(ref v) = update.artist_name {
        map.insert("artist_name".into(), v.clone());
    }
    if let Some(ref v) = update.album_title {
        map.insert("album_title".into(), v.clone());
    }
    if let Some(v) = update.track_number {
        map.insert("track_number".into(), v.to_string());
    }
    if let Some(v) = update.disc_number {
        map.insert("disc_number".into(), v.to_string());
    }
    if let Some(ref v) = update.genre {
        map.insert("genre".into(), v.clone());
    }
    if let Some(ref v) = update.composer {
        map.insert("composer".into(), v.clone());
    }
    if let Some(v) = update.year {
        map.insert("year".into(), v.to_string());
    }
    if let Some(ref v) = update.comment {
        map.insert("comment".into(), v.clone());
    }
    if let Some(ref v) = update.isrc {
        map.insert("isrc".into(), v.clone());
    }
    if let Some(v) = update.bpm {
        map.insert("bpm".into(), v.to_string());
    }
    if let Some(ref v) = update.label {
        map.insert("label".into(), v.clone());
    }
    map
}

pub async fn read_tags(file_path: &str) -> Result<HashMap<String, String>, String> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            file_path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("ffprobe: {e}"))?;

    if !output.status.success() {
        return Err("ffprobe failed".into());
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("parse: {e}"))?;

    let tags = json["format"]["tags"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.to_lowercase(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    Ok(tags)
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    pub file_path: String,
    pub fields_written: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_mp3() {
        assert_eq!(detect_format("/music/song.mp3"), TagFormat::Id3);
    }

    #[test]
    fn detect_flac() {
        assert_eq!(detect_format("/music/song.flac"), TagFormat::Vorbis);
    }

    #[test]
    fn detect_m4a() {
        assert_eq!(detect_format("/music/song.m4a"), TagFormat::Mp4);
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(detect_format("/music/song.xyz"), TagFormat::Unknown);
    }

    #[test]
    fn build_metadata_id3() {
        let update = TagUpdate {
            title: Some("Test Song".into()),
            artist_name: Some("Test Artist".into()),
            ..Default::default()
        };
        let meta = build_ffmpeg_metadata(&update, TagFormat::Id3);
        assert!(meta.iter().any(|(k, v)| k == "TIT2" && v == "Test Song"));
        assert!(meta.iter().any(|(k, v)| k == "TPE1" && v == "Test Artist"));
    }

    #[test]
    fn build_metadata_vorbis() {
        let update = TagUpdate {
            title: Some("Test".into()),
            genre: Some("Rock".into()),
            ..Default::default()
        };
        let meta = build_ffmpeg_metadata(&update, TagFormat::Vorbis);
        assert!(meta.iter().any(|(k, _)| k == "TITLE"));
        assert!(meta.iter().any(|(k, _)| k == "GENRE"));
    }

    #[test]
    fn empty_update_no_metadata() {
        let update = TagUpdate::default();
        let meta = build_ffmpeg_metadata(&update, TagFormat::Id3);
        assert!(meta.is_empty());
    }

    #[test]
    fn tag_update_to_map_partial() {
        let update = TagUpdate {
            title: Some("Song".into()),
            year: Some(2024),
            ..Default::default()
        };
        let map = tag_update_to_map(&update);
        assert_eq!(map.len(), 2);
        assert_eq!(map["title"], "Song");
        assert_eq!(map["year"], "2024");
    }
}
