use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportedTrack {
    pub file_path: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub play_count: i32,
    pub rating: Option<i32>,
    pub date_added: Option<String>,
    pub last_played: Option<String>,
    pub playlist_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub imported_title: String,
    pub imported_artist: Option<String>,
    pub imported_album: Option<String>,
    pub tune_track_id: Option<i64>,
    pub matched: bool,
    pub match_method: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportReport {
    pub source: String,
    pub total_rows: usize,
    pub matched: usize,
    pub unmatched: usize,
    pub play_counts_updated: usize,
    pub ratings_updated: usize,
    pub playlists_created: usize,
}

// -- Roon CSV parser --

const ROON_PATH_HEADERS: &[&str] = &["file path", "filepath", "path", "file_path", "location", "file"];
const ROON_TITLE_HEADERS: &[&str] = &["title", "track title", "track_title", "track", "name"];
const ROON_ARTIST_HEADERS: &[&str] = &["artist", "artist name", "artist_name", "performers"];
const ROON_ALBUM_HEADERS: &[&str] = &["album", "album title", "album_title"];
const ROON_PLAY_COUNT_HEADERS: &[&str] = &["play count", "play_count", "plays", "playcount"];
const ROON_RATING_HEADERS: &[&str] = &["rating", "stars", "user rating", "user_rating"];

fn detect_header(headers: &[String], candidates: &[&str]) -> Option<usize> {
    for (i, h) in headers.iter().enumerate() {
        if candidates.contains(&h.as_str()) {
            return Some(i);
        }
    }
    None
}

pub fn parse_roon_csv(raw: &str) -> Vec<ImportedTrack> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(raw.as_bytes());

    let headers: Vec<String> = match reader.headers() {
        Ok(h) => h.iter().map(|s| s.trim().trim_start_matches('\u{feff}').to_lowercase()).collect(),
        Err(_) => return Vec::new(),
    };

    let col_path = detect_header(&headers, ROON_PATH_HEADERS);
    let col_title = detect_header(&headers, ROON_TITLE_HEADERS);
    let col_artist = detect_header(&headers, ROON_ARTIST_HEADERS);
    let col_album = detect_header(&headers, ROON_ALBUM_HEADERS);
    let col_play = detect_header(&headers, ROON_PLAY_COUNT_HEADERS);
    let col_rating = detect_header(&headers, ROON_RATING_HEADERS);

    if col_title.is_none() && col_path.is_none() {
        warn!(headers = ?headers, "import_roon_csv_no_usable_headers");
        return Vec::new();
    }

    let mut tracks = Vec::new();
    for result in reader.records() {
        let record = match result {
            Ok(r) => r,
            Err(_) => continue,
        };

        let mut t = ImportedTrack::default();

        if let Some(i) = col_path {
            t.file_path = record.get(i).and_then(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s.to_string()) }
            });
        }
        if let Some(i) = col_title {
            t.title = record.get(i).and_then(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s.to_string()) }
            });
        }
        if let Some(i) = col_artist {
            t.artist = record.get(i).and_then(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s.to_string()) }
            });
        }
        if let Some(i) = col_album {
            t.album = record.get(i).and_then(|s| {
                let s = s.trim();
                if s.is_empty() { None } else { Some(s.to_string()) }
            });
        }
        if let Some(i) = col_play {
            if let Some(s) = record.get(i) {
                t.play_count = s.trim().parse().unwrap_or(0);
            }
        }
        if let Some(i) = col_rating {
            if let Some(s) = record.get(i) {
                if let Ok(r) = s.trim().parse::<f64>() {
                    let r = r as i32;
                    if r > 0 {
                        t.rating = Some(r.clamp(1, 5));
                    }
                }
            }
        }

        if t.title.is_some() || t.file_path.is_some() {
            tracks.push(t);
        }
    }

    info!(count = tracks.len(), "roon_csv_parsed");
    tracks
}

// -- Plex XML parser --

pub fn parse_plex_xml(raw: &str) -> Vec<ImportedTrack> {
    let mut tracks = Vec::new();

    let mut reader = quick_xml::Reader::from_str(raw);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Empty(ref e)) | Ok(quick_xml::events::Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_lowercase();
                if tag != "track" && tag != "video" {
                    continue;
                }

                let mut t = ImportedTrack::default();
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.as_ref()).to_lowercase();
                    let val = String::from_utf8_lossy(&attr.value).to_string();
                    match key.as_str() {
                        "title" => t.title = Some(val),
                        "grandparenttitle" | "originaltitle" => {
                            if t.artist.is_none() {
                                t.artist = Some(val);
                            }
                        }
                        "parenttitle" => t.album = Some(val),
                        "viewcount" => t.play_count = val.parse().unwrap_or(0),
                        "userrating" => {
                            if let Ok(r) = val.parse::<f64>() {
                                let stars = (r / 2.0 + 0.5) as i32;
                                if stars > 0 {
                                    t.rating = Some(stars.clamp(1, 5));
                                }
                            }
                        }
                        _ => {}
                    }
                }

                if t.title.is_some() || t.file_path.is_some() {
                    tracks.push(t);
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    info!(count = tracks.len(), "plex_xml_parsed");
    tracks
}

// -- M3U parser --

pub fn parse_m3u(raw: &str, playlist_name: Option<&str>) -> Vec<ImportedTrack> {
    let mut tracks = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_artist: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line == "#EXTM3U" {
            continue;
        }
        if line.starts_with("#EXTINF:") {
            if let Some(comma) = line.find(',') {
                let display = line[comma + 1..].trim();
                if let Some(sep) = display.find(" - ") {
                    current_artist = Some(display[..sep].trim().to_string());
                    current_title = Some(display[sep + 3..].trim().to_string());
                } else {
                    current_title = Some(display.to_string());
                }
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        tracks.push(ImportedTrack {
            file_path: Some(line.to_string()),
            title: current_title.take(),
            artist: current_artist.take(),
            playlist_name: playlist_name.map(String::from),
            ..Default::default()
        });
    }

    tracks
}

// -- PLS parser --

pub fn parse_pls(raw: &str, playlist_name: Option<&str>) -> Vec<ImportedTrack> {
    let mut files: HashMap<String, String> = HashMap::new();
    let mut titles: HashMap<String, String> = HashMap::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value.trim().to_string();

        if let Some(idx) = key.strip_prefix("file") {
            files.insert(idx.to_string(), value);
        } else if let Some(idx) = key.strip_prefix("title") {
            titles.insert(idx.to_string(), value);
        }
    }

    let mut indices: Vec<String> = files.keys().cloned().collect();
    indices.sort_by_key(|k| k.parse::<i32>().unwrap_or(0));

    indices
        .into_iter()
        .map(|idx| {
            let fp = files.get(&idx).cloned().unwrap_or_default();
            let display = titles.get(&idx).cloned().unwrap_or_default();
            let (artist, title) = if let Some(sep) = display.find(" - ") {
                (
                    Some(display[..sep].trim().to_string()),
                    Some(display[sep + 3..].trim().to_string()),
                )
            } else if !display.is_empty() {
                (None, Some(display))
            } else {
                (None, None)
            };

            ImportedTrack {
                file_path: Some(fp),
                title,
                artist,
                playlist_name: playlist_name.map(String::from),
                ..Default::default()
            }
        })
        .collect()
}

// -- Matching engine --

pub fn match_tracks(
    imported: &[ImportedTrack],
    path_index: &HashMap<String, i64>,
    fuzzy_index: &HashMap<(String, String, String), i64>,
) -> Vec<MatchResult> {
    imported
        .iter()
        .map(|imp| {
            let mut mr = MatchResult {
                imported_title: imp.title.clone().unwrap_or_else(|| "(sans titre)".into()),
                imported_artist: imp.artist.clone(),
                imported_album: imp.album.clone(),
                tune_track_id: None,
                matched: false,
                match_method: None,
            };

            // 1. Exact file_path
            if let Some(ref fp) = imp.file_path {
                if let Some(&tid) = path_index.get(fp) {
                    mr.tune_track_id = Some(tid);
                    mr.matched = true;
                    mr.match_method = Some("file_path".into());
                    return mr;
                }
                let normalized = fp.replace('\\', "/");
                if let Some(&tid) = path_index.get(&normalized) {
                    mr.tune_track_id = Some(tid);
                    mr.matched = true;
                    mr.match_method = Some("file_path".into());
                    return mr;
                }
            }

            // 2. Fuzzy: title + artist + album
            let key = (
                imp.title.as_deref().unwrap_or("").to_lowercase(),
                imp.artist.as_deref().unwrap_or("").to_lowercase(),
                imp.album.as_deref().unwrap_or("").to_lowercase(),
            );

            if !key.0.is_empty() {
                if let Some(&tid) = fuzzy_index.get(&key) {
                    mr.tune_track_id = Some(tid);
                    mr.matched = true;
                    mr.match_method = Some("fuzzy".into());
                    return mr;
                }

                // Relax: title + artist only
                if !key.1.is_empty() {
                    for (fk, &fid) in fuzzy_index {
                        if fk.0 == key.0 && fk.1 == key.1 {
                            mr.tune_track_id = Some(fid);
                            mr.matched = true;
                            mr.match_method = Some("fuzzy".into());
                            return mr;
                        }
                    }
                }

                // Relax: title only (unique match)
                let candidates: Vec<i64> = fuzzy_index
                    .iter()
                    .filter(|(fk, _)| fk.0 == key.0)
                    .map(|(_, &fid)| fid)
                    .collect();
                if candidates.len() == 1 {
                    mr.tune_track_id = Some(candidates[0]);
                    mr.matched = true;
                    mr.match_method = Some("fuzzy".into());
                }
            }

            mr
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roon_csv_basic() {
        let csv = "Title,Artist,Album,Play Count\nSong One,Artist A,Album X,5\nSong Two,Artist B,,3\n";
        let tracks = parse_roon_csv(csv);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title.as_deref(), Some("Song One"));
        assert_eq!(tracks[0].artist.as_deref(), Some("Artist A"));
        assert_eq!(tracks[0].play_count, 5);
        assert_eq!(tracks[1].album, None);
    }

    #[test]
    fn parse_roon_csv_empty() {
        let tracks = parse_roon_csv("");
        assert!(tracks.is_empty());
    }

    #[test]
    fn parse_roon_csv_no_usable_headers() {
        let csv = "Foo,Bar\n1,2\n";
        let tracks = parse_roon_csv(csv);
        assert!(tracks.is_empty());
    }

    #[test]
    fn parse_m3u_basic() {
        let m3u = "#EXTM3U\n#EXTINF:180,Pink Floyd - Time\n/music/time.flac\n#EXTINF:200,Comfortably Numb\n/music/numb.flac\n";
        let tracks = parse_m3u(m3u, Some("My Playlist"));
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].artist.as_deref(), Some("Pink Floyd"));
        assert_eq!(tracks[0].title.as_deref(), Some("Time"));
        assert_eq!(tracks[0].file_path.as_deref(), Some("/music/time.flac"));
        assert_eq!(tracks[0].playlist_name.as_deref(), Some("My Playlist"));
        assert_eq!(tracks[1].title.as_deref(), Some("Comfortably Numb"));
        assert!(tracks[1].artist.is_none());
    }

    #[test]
    fn parse_pls_basic() {
        let pls = "[playlist]\nFile1=/music/a.flac\nTitle1=Artist - Song\nFile2=/music/b.mp3\nTitle2=Just Title\nNumberOfEntries=2\n";
        let tracks = parse_pls(pls, None);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].artist.as_deref(), Some("Artist"));
        assert_eq!(tracks[0].title.as_deref(), Some("Song"));
        assert_eq!(tracks[1].title.as_deref(), Some("Just Title"));
        assert!(tracks[1].artist.is_none());
    }

    #[test]
    fn parse_plex_xml_basic() {
        let xml = r#"<MediaContainer><Track title="Song" grandparentTitle="Artist" parentTitle="Album" viewCount="3" /><Track title="Another" /></MediaContainer>"#;
        let tracks = parse_plex_xml(xml);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title.as_deref(), Some("Song"));
        assert_eq!(tracks[0].artist.as_deref(), Some("Artist"));
        assert_eq!(tracks[0].album.as_deref(), Some("Album"));
        assert_eq!(tracks[0].play_count, 3);
    }

    #[test]
    fn match_exact_path() {
        let imported = vec![ImportedTrack {
            file_path: Some("/music/song.flac".into()),
            title: Some("Song".into()),
            ..Default::default()
        }];
        let mut path_idx = HashMap::new();
        path_idx.insert("/music/song.flac".into(), 42_i64);
        let fuzzy_idx = HashMap::new();

        let results = match_tracks(&imported, &path_idx, &fuzzy_idx);
        assert_eq!(results.len(), 1);
        assert!(results[0].matched);
        assert_eq!(results[0].tune_track_id, Some(42));
        assert_eq!(results[0].match_method.as_deref(), Some("file_path"));
    }

    #[test]
    fn match_fuzzy_title_artist() {
        let imported = vec![ImportedTrack {
            title: Some("Time".into()),
            artist: Some("Pink Floyd".into()),
            ..Default::default()
        }];
        let path_idx = HashMap::new();
        let mut fuzzy_idx = HashMap::new();
        fuzzy_idx.insert(
            ("time".into(), "pink floyd".into(), "dark side".into()),
            99_i64,
        );

        let results = match_tracks(&imported, &path_idx, &fuzzy_idx);
        assert!(results[0].matched);
        assert_eq!(results[0].tune_track_id, Some(99));
    }

    #[test]
    fn match_no_match() {
        let imported = vec![ImportedTrack {
            title: Some("Unknown".into()),
            ..Default::default()
        }];
        let results = match_tracks(&imported, &HashMap::new(), &HashMap::new());
        assert!(!results[0].matched);
        assert!(results[0].tune_track_id.is_none());
    }

    #[test]
    fn match_windows_path_normalization() {
        let imported = vec![ImportedTrack {
            file_path: Some("C:\\Music\\song.flac".into()),
            title: Some("Song".into()),
            ..Default::default()
        }];
        let mut path_idx = HashMap::new();
        path_idx.insert("C:/Music/song.flac".into(), 7_i64);

        let results = match_tracks(&imported, &path_idx, &HashMap::new());
        assert!(results[0].matched);
    }
}
