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
        // Skip metadata tags like [ti:Title], [ar:Artist]
        if line.starts_with('[') && line.contains(':') && !line[1..].starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        // Parse [mm:ss.xx] text
        if let Some(bracket_end) = line.find(']') {
            let timestamp = &line[1..bracket_end];
            let text = line[bracket_end + 1..].trim().to_string();
            if let Some(ms) = parse_lrc_timestamp(timestamp) {
                lines.push(LrcLine { time_ms: ms, text });
            }
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
    let minutes: u64 = parts[0].parse().ok()?;
    let sec_parts: Vec<&str> = parts[1].split('.').collect();
    let seconds: u64 = sec_parts[0].parse().ok()?;
    let centiseconds: u64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1];
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

pub fn find_sidecar_lrc(audio_path: &str) -> Option<String> {
    let path = std::path::Path::new(audio_path);
    let lrc_path = path.with_extension("lrc");
    if lrc_path.exists() {
        std::fs::read_to_string(&lrc_path).ok()
    } else {
        None
    }
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
        let content = "[ti:Song Title]\n[ar:Artist]\n[00:05.00] Actual lyrics";
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
    fn sidecar_nonexistent() {
        assert!(find_sidecar_lrc("/nonexistent/track.flac").is_none());
    }
}
