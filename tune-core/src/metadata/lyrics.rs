//! LRC lyrics support: parser, sidecar discovery, embedded-tag reading.
//!
//! Canonical LRC parser for the whole codebase (the LRCLIB module in
//! `crate::lyrics` delegates here). Handles:
//! - `[mm:ss.xx]` timestamps with 1-3 fractional digits (centiseconds or
//!   milliseconds),
//! - several timestamps on one line (`[00:12.00][01:15.00]chorus` yields
//!   two entries),
//! - metadata tags (`[ar:..]`, `[ti:..]`, `[offset:..]`…) which are ignored,
//! - output sorted by `time_ms`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LrcLine {
    pub time_ms: u64,
    pub text: String,
}

pub fn parse_lrc(content: &str) -> Vec<LrcLine> {
    let mut lines = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Collect every leading `[mm:ss.xx]` timestamp. Metadata tags like
        // `[ti:Title]` fail to parse as timestamps and skip the whole line.
        let mut rest = line;
        let mut stamps: Vec<u64> = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else { break };
            let Some(ms) = parse_lrc_timestamp(&after[..end]) else {
                break;
            };
            stamps.push(ms);
            rest = after[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        let text = rest.trim().to_string();
        for ms in stamps {
            lines.push(LrcLine {
                time_ms: ms,
                text: text.clone(),
            });
        }
    }
    lines.sort_by_key(|l| l.time_ms);
    lines
}

fn parse_lrc_timestamp(ts: &str) -> Option<u64> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let minutes: u64 = parts[0].trim().parse().ok()?;
    let sec_parts: Vec<&str> = parts[1].split('.').collect();
    let seconds: u64 = sec_parts[0].trim().parse().ok()?;
    let centiseconds: u64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1].trim();
        let val: u64 = frac.parse().ok()?;
        match frac.len() {
            1 => val * 100,
            2 => val * 10,
            3 => val,
            _ => val / 10u64.pow(frac.len() as u32 - 3),
        }
    } else {
        0
    };
    Some(minutes * 60_000 + seconds * 1000 + centiseconds)
}

/// True when the text embeds at least one parseable LRC timestamp
/// (used to detect LRC content stored inside a USLT/LYRICS tag).
pub fn has_lrc_timestamps(content: &str) -> bool {
    !parse_lrc(content).is_empty()
}

/// Look for a sidecar `.lrc` next to the audio file (same stem). Tries the
/// lowercase `.lrc` extension first, then uppercase `.LRC`. Read-only:
/// never writes anything into the user's music folders.
pub fn find_sidecar_lrc(audio_path: &str) -> Option<String> {
    let path = std::path::Path::new(audio_path);
    for ext in ["lrc", "LRC"] {
        let candidate = path.with_extension(ext);
        if candidate.exists() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                return Some(content);
            }
        }
    }
    None
}

/// Read lyrics embedded in the audio file's tags (USLT for ID3, LYRICS for
/// Vorbis comments…) via lofty — same mechanics as the scanner's
/// `read_extended_metadata`, restricted to the lyrics item and skipping
/// cover art to keep memory flat.
pub fn read_embedded_lyrics(audio_path: &str) -> Option<String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::ItemKey;

    let tagged = Probe::open(audio_path)
        .and_then(|p| {
            p.options(
                ParseOptions::new()
                    .parsing_mode(ParsingMode::Relaxed)
                    .max_junk_bytes(1024 * 1024)
                    .read_cover_art(false),
            )
            .guess_file_type()?
            .read()
        })
        .ok()?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    tag.get_string(ItemKey::Lyrics)
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_lrc() {
        let content = "[00:12.50] First line\n[00:25.30] Second line\n[01:00.00] Third line";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 12_500);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].time_ms, 25_300);
        assert_eq!(lines[2].time_ms, 60_000);
    }

    #[test]
    fn skip_metadata_tags() {
        let content =
            "[ti:Song Title]\n[ar:Artist]\n[al:Album]\n[offset:+500]\n[00:05.00] Actual lyrics";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Actual lyrics");
    }

    #[test]
    fn empty_input() {
        assert!(parse_lrc("").is_empty());
        assert!(parse_lrc("   \n\n  ").is_empty());
    }

    #[test]
    fn three_digit_milliseconds() {
        let content = "[01:23.456] Precise timing";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 83_456);
    }

    #[test]
    fn two_digit_centiseconds() {
        let content = "[00:12.34] Centi";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 12_340);
    }

    #[test]
    fn no_fractional_seconds() {
        let content = "[02:30] No fraction";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 150_000);
    }

    #[test]
    fn sorted_output() {
        let content = "[01:00.00] Later\n[00:30.00] Earlier";
        let lines = parse_lrc(content);
        assert_eq!(lines[0].text, "Earlier");
        assert_eq!(lines[1].text, "Later");
    }

    #[test]
    fn multiple_timestamps_one_line() {
        let content = "[00:12.00][01:15.00]Chorus line\n[00:30.00] Verse";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 3);
        // Sorted: 12s chorus, 30s verse, 75s chorus.
        assert_eq!(lines[0].time_ms, 12_000);
        assert_eq!(lines[0].text, "Chorus line");
        assert_eq!(lines[1].time_ms, 30_000);
        assert_eq!(lines[1].text, "Verse");
        assert_eq!(lines[2].time_ms, 75_000);
        assert_eq!(lines[2].text, "Chorus line");
    }

    #[test]
    fn garbage_bracket_is_skipped() {
        let lines = parse_lrc("[bad] nope\nnot a timestamp");
        assert!(lines.is_empty());
    }

    #[test]
    fn detects_lrc_in_tag_content() {
        assert!(has_lrc_timestamps("[00:01.00] hey"));
        assert!(!has_lrc_timestamps("Plain lyrics\nSecond line"));
        assert!(!has_lrc_timestamps("[ar:Someone]\nstill plain"));
    }

    #[test]
    fn sidecar_nonexistent() {
        assert!(find_sidecar_lrc("/nonexistent/track.flac").is_none());
    }

    #[test]
    fn sidecar_uppercase_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        let audio = dir.path().join("Song.flac");
        std::fs::write(dir.path().join("Song.LRC"), "[00:01.00] up").unwrap();
        let content = find_sidecar_lrc(audio.to_str().unwrap());
        assert_eq!(content.as_deref(), Some("[00:01.00] up"));
    }
}
