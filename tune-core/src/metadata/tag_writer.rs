use std::collections::HashMap;
use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

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
    let path = file_path.to_string();
    let update = update.clone();
    tokio::task::spawn_blocking(move || write_tags_lofty(&path, &update))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn write_tags_lofty(file_path: &str, update: &TagUpdate) -> Result<WriteResult, String> {
    let mut tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let tag_type = tagged.primary_tag_type();

    if tagged.primary_tag().is_none() && tagged.first_tag().is_none() {
        tagged.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let has_primary = tagged.primary_tag().is_some();
    let tag = if has_primary {
        tagged.primary_tag_mut().unwrap()
    } else {
        tagged.first_tag_mut().unwrap()
    };

    let mut count = 0usize;
    if let Some(ref v) = update.title {
        tag.set_title(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.artist_name {
        tag.set_artist(v.clone());
        count += 1;
    }
    if let Some(ref v) = update.album_title {
        tag.set_album(v.clone());
        count += 1;
    }
    if let Some(v) = update.track_number {
        tag.set_track(v as u32);
        count += 1;
    }
    if let Some(v) = update.disc_number {
        tag.set_disk(v as u32);
        count += 1;
    }
    if let Some(ref v) = update.genre {
        tag.set_genre(v.clone());
        count += 1;
    }
    if let Some(v) = update.year {
        tag.set_date(lofty::tag::items::Timestamp {
            year: v as u16,
            ..Default::default()
        });
        count += 1;
    }
    if let Some(ref v) = update.comment {
        tag.set_comment(v.clone());
        count += 1;
    }

    if count == 0 {
        return Ok(WriteResult {
            file_path: file_path.into(),
            fields_written: 0,
        });
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_path)
        .map_err(|e| format!("open: {e}"))?;
    tagged
        .save_to(&mut file, WriteOptions::default())
        .map_err(|e| format!("lofty save: {e}"))?;

    info!(file = file_path, fields = count, "tags_written_lofty");
    Ok(WriteResult {
        file_path: file_path.into(),
        fields_written: count,
    })
}

pub async fn read_tags(file_path: &str) -> Result<HashMap<String, String>, String> {
    let path = file_path.to_string();
    tokio::task::spawn_blocking(move || read_tags_lofty(&path))
        .await
        .map_err(|e| format!("join: {e}"))?
}

fn read_tags_lofty(file_path: &str) -> Result<HashMap<String, String>, String> {
    let tagged = lofty::read_from_path(file_path).map_err(|e| format!("lofty read: {e}"))?;
    let mut tags = HashMap::new();
    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return Ok(tags),
    };
    if let Some(v) = tag.title() {
        tags.insert("title".into(), v.to_string());
    }
    if let Some(v) = tag.artist() {
        tags.insert("artist".into(), v.to_string());
    }
    if let Some(v) = tag.album() {
        tags.insert("album".into(), v.to_string());
    }
    if let Some(v) = tag.genre() {
        tags.insert("genre".into(), v.to_string());
    }
    if let Some(v) = tag.date() {
        tags.insert("date".into(), v.to_string());
    }
    if let Some(v) = tag.track() {
        tags.insert("tracknumber".into(), v.to_string());
    }
    if let Some(v) = tag.disk() {
        tags.insert("discnumber".into(), v.to_string());
    }
    if let Some(v) = tag.comment() {
        tags.insert("comment".into(), v.to_string());
    }
    debug!(file = file_path, count = tags.len(), "tags_read_lofty");
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
}
