use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3UEntry {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_s: i32,
    pub is_url: bool,
    pub extra_attrs: HashMap<String, String>,
}

pub fn parse_m3u_content(raw: &[u8], force_utf8: bool) -> Vec<M3UEntry> {
    let text = decode(raw, force_utf8);
    parse_text(&text)
}

fn decode(raw: &[u8], force_utf8: bool) -> String {
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&raw[3..]).into_owned();
    }
    if force_utf8 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| b as char).collect(),
    }
}

fn parse_text(text: &str) -> Vec<M3UEntry> {
    // PLS is a distinct format ([playlist] with FileN=/TitleN= keys). The radio
    // import dialog accepts .pls, but the M3U grammar below treats `File1=url`
    // as a non-URL path, so a .pls yielded zero importable stations (Dominique
    // COMET: "Importer M3U/PLS ajoute 0 radios"). Detect and parse PLS here.
    if is_pls(text) {
        return parse_pls(text);
    }

    let mut entries = Vec::new();
    let mut pending_title: Option<String> = None;
    let mut pending_artist: Option<String> = None;
    let mut pending_duration: i32 = -1;
    let mut pending_attrs: HashMap<String, String> = HashMap::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let upper = line.to_uppercase();
        if upper.starts_with("#EXTM3U") {
            continue;
        }

        if upper.starts_with("#EXTINF:") {
            let (dur, artist, title, attrs) = parse_extinf(line);
            pending_duration = dur;
            pending_artist = artist;
            pending_title = title;
            pending_attrs = attrs;
            continue;
        }

        if line.starts_with('#') {
            continue;
        }

        let is_url = is_url_path(line);
        entries.push(M3UEntry {
            path: line.to_string(),
            title: pending_title.take(),
            artist: pending_artist.take(),
            duration_s: pending_duration,
            is_url,
            extra_attrs: std::mem::take(&mut pending_attrs),
        });
        pending_duration = -1;
    }

    entries
}

/// Does this look like a PLS playlist ([playlist] header or `FileN=` keys)?
fn is_pls(text: &str) -> bool {
    text.lines().take(20).any(|l| {
        let t = l.trim();
        if t.eq_ignore_ascii_case("[playlist]") {
            return true;
        }
        if let Some(rest) = t.to_ascii_lowercase().strip_prefix("file") {
            return rest.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('=');
        }
        false
    })
}

/// Parse PLS content, pairing `FileN=<url>` with `TitleN=<name>` by index.
fn parse_pls(text: &str) -> Vec<M3UEntry> {
    use std::collections::BTreeMap;
    let mut files: BTreeMap<u32, String> = BTreeMap::new();
    let mut titles: BTreeMap<u32, String> = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim().to_ascii_lowercase();
        let val = line[eq + 1..].trim().to_string();
        if val.is_empty() {
            continue;
        }
        if let Some(digits) = key.strip_prefix("file") {
            if let Ok(idx) = digits.parse::<u32>() {
                files.insert(idx, val);
            }
        } else if let Some(digits) = key.strip_prefix("title") {
            if let Ok(idx) = digits.parse::<u32>() {
                titles.insert(idx, val);
            }
        }
    }
    files
        .into_iter()
        .map(|(idx, url)| {
            let is_url = is_url_path(&url);
            M3UEntry {
                path: url,
                title: titles.get(&idx).cloned(),
                artist: None,
                duration_s: -1,
                is_url,
                extra_attrs: HashMap::new(),
            }
        })
        .collect()
}

fn parse_extinf(line: &str) -> (i32, Option<String>, Option<String>, HashMap<String, String>) {
    let rest = &line[8..]; // skip "#EXTINF:"

    let mut attrs = HashMap::new();
    let attr_re = regex::Regex::new(r#"(\w[\w-]*)="([^"]*)""#).unwrap();
    for cap in attr_re.captures_iter(rest) {
        attrs.insert(cap[1].to_lowercase(), cap[2].to_string());
    }

    let comma_idx = match rest.find(',') {
        Some(idx) => idx,
        None => {
            let dur = parse_duration_token(rest.split_whitespace().next().unwrap_or("-1"));
            return (dur, None, None, attrs);
        }
    };

    let before_comma = &rest[..comma_idx];
    let display_name = rest[comma_idx + 1..].trim();

    let dur_token = before_comma.split_whitespace().next().unwrap_or("-1");
    let duration = parse_duration_token(dur_token);

    let (artist, title) = split_artist_title(display_name);
    (duration, artist, title, attrs)
}

fn parse_duration_token(token: &str) -> i32 {
    let cleaned = token.trim_end_matches(',').trim();
    cleaned
        .parse::<f64>()
        .ok()
        .map(|v| {
            let i = v as i32;
            if i >= 0 { i } else { -1 }
        })
        .unwrap_or(-1)
}

fn split_artist_title(display: &str) -> (Option<String>, Option<String>) {
    if display.is_empty() {
        return (None, None);
    }
    for sep in [" - ", " -- ", " – ", " — "] {
        if let Some(idx) = display.find(sep) {
            let artist = display[..idx].trim();
            let title = display[idx + sep.len()..].trim();
            let a = if artist.is_empty() {
                None
            } else {
                Some(artist.to_string())
            };
            let t = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            return (a, t);
        }
    }
    (None, Some(display.to_string()))
}

fn is_url_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("rtsp://")
        || lower.starts_with("mms://")
        || lower.starts_with("rtmp://")
}

pub struct M3UTrack {
    pub title: String,
    pub artist_name: Option<String>,
    pub duration_ms: i64,
    pub file_path: Option<String>,
    pub source: String,
    pub source_id: Option<String>,
}

pub fn generate_m3u8(entries: &[M3UTrack]) -> String {
    let mut lines = vec!["#EXTM3U".to_string()];

    for entry in entries {
        let duration_s = if entry.duration_ms > 0 {
            (entry.duration_ms / 1000) as i32
        } else {
            -1
        };

        let display = match &entry.artist_name {
            Some(artist) if !artist.is_empty() => format!("{artist} - {}", entry.title),
            _ => entry.title.clone(),
        };

        lines.push(format!("#EXTINF:{duration_s},{display}"));

        if let Some(ref path) = entry.file_path {
            lines.push(path.clone());
        } else if let Some(ref sid) = entry.source_id {
            if entry.source != "local" {
                lines.push(format!("# {}:{sid}", entry.source));
            } else {
                lines.push(format!("# {}", entry.title));
            }
        } else {
            lines.push(format!("# {}", entry.title));
        }
    }

    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_m3u() {
        let content = b"#EXTM3U\n#EXTINF:180,Artist - Title\n/music/track.flac\n";
        let entries = parse_m3u_content(content, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].artist.as_deref(), Some("Artist"));
        assert_eq!(entries[0].title.as_deref(), Some("Title"));
        assert_eq!(entries[0].duration_s, 180);
        assert!(!entries[0].is_url);
    }

    #[test]
    fn parse_no_artist() {
        let content = b"#EXTINF:120,Just A Title\n/music/file.mp3\n";
        let entries = parse_m3u_content(content, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].artist, None);
        assert_eq!(entries[0].title.as_deref(), Some("Just A Title"));
    }

    #[test]
    fn parse_url_entry() {
        let content = b"#EXTINF:-1,Radio Station\nhttp://stream.example.com/radio.mp3\n";
        let entries = parse_m3u_content(content, false);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_url);
        assert_eq!(entries[0].duration_s, -1);
    }

    #[test]
    fn parse_pls_playlist() {
        let content = b"[playlist]\nNumberOfEntries=2\n\
                        File1=http://stream.example.com/radio1.mp3\nTitle1=Radio One\nLength1=-1\n\
                        File2=https://stream.example.com/radio2\nTitle2=Radio Two\n\
                        Version=2\n";
        let entries = parse_m3u_content(content, true);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_url);
        assert_eq!(entries[0].path, "http://stream.example.com/radio1.mp3");
        assert_eq!(entries[0].title.as_deref(), Some("Radio One"));
        assert_eq!(entries[1].title.as_deref(), Some("Radio Two"));
        assert!(entries[1].is_url);
    }

    #[test]
    fn parse_with_attrs() {
        let content = b"#EXTINF:-1 tvg-logo=\"http://logo.png\" group-title=\"Music\",Station\n\
                        http://stream.example.com\n";
        let entries = parse_m3u_content(content, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].extra_attrs.get("tvg-logo").unwrap(),
            "http://logo.png"
        );
        assert_eq!(entries[0].extra_attrs.get("group-title").unwrap(), "Music");
    }

    #[test]
    fn parse_bom_utf8() {
        let mut content = vec![0xEF, 0xBB, 0xBF];
        content.extend_from_slice(b"#EXTM3U\n#EXTINF:60,Test\n/file.flac\n");
        let entries = parse_m3u_content(&content, false);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn parse_latin1_fallback() {
        let content: Vec<u8> = b"#EXTINF:60,R\xe9sum\xe9 - Caf\xe9\n/file.mp3\n".to_vec();
        let entries = parse_m3u_content(&content, false);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].artist.is_some());
    }

    #[test]
    fn generate_m3u8_roundtrip() {
        let tracks = vec![
            M3UTrack {
                title: "Song One".into(),
                artist_name: Some("Artist A".into()),
                duration_ms: 180_000,
                file_path: Some("/music/song1.flac".into()),
                source: "local".into(),
                source_id: None,
            },
            M3UTrack {
                title: "Song Two".into(),
                artist_name: None,
                duration_ms: 0,
                file_path: None,
                source: "radio".into(),
                source_id: Some("station123".into()),
            },
        ];
        let output = generate_m3u8(&tracks);
        assert!(output.starts_with("#EXTM3U\n"));
        assert!(output.contains("#EXTINF:180,Artist A - Song One"));
        assert!(output.contains("/music/song1.flac"));
        assert!(output.contains("# radio:station123"));
    }

    #[test]
    fn split_separators() {
        let (a, t) = split_artist_title("Artist – Title");
        assert_eq!(a.as_deref(), Some("Artist"));
        assert_eq!(t.as_deref(), Some("Title"));

        let (a, t) = split_artist_title("Only Title");
        assert_eq!(a, None);
        assert_eq!(t.as_deref(), Some("Only Title"));
    }

    #[test]
    fn multiple_entries() {
        let content = b"#EXTM3U\n\
            #EXTINF:100,A1 - T1\n/a.flac\n\
            #EXTINF:200,A2 - T2\n/b.flac\n\
            /c.mp3\n";
        let entries = parse_m3u_content(content, false);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].title, None);
        assert_eq!(entries[2].duration_s, -1);
    }
}
