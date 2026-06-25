//! Synchronized lyrics via LRCLIB.
//!
//! - Parses LRC-format timestamped lyrics into `Vec<LyricLine>`.
//! - Fetches from <https://lrclib.net/api/get> (no API key required).
//! - Caches results in `lyrics_cache` DB table.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::db::backend::{DbBackend, ToSqlValue};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single time-stamped lyric line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricLine {
    /// Milliseconds from track start.
    pub time_ms: i64,
    /// The lyric text for this line.
    pub text: String,
}

/// Full lyrics payload returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lyrics {
    /// Whether time-synced lyrics are available.
    pub synced: bool,
    /// Parsed time-stamped lines (empty when `synced` is false).
    pub lines: Vec<LyricLine>,
    /// Plain (unsynced) lyrics text.
    pub plain_text: Option<String>,
    /// Attribution source.
    pub source: String,
}

// ---------------------------------------------------------------------------
// LRCLIB API response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

// ---------------------------------------------------------------------------
// LRC parser
// ---------------------------------------------------------------------------

/// Parse an LRC-format string into a sorted `Vec<LyricLine>`.
///
/// Accepted format per line: `[MM:SS.xx] text` where `xx` can be 1-3
/// digits (centiseconds or milliseconds).
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();

    for raw in lrc.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }

        // Find `[MM:SS.xx]` prefix.
        let Some(close) = raw.find(']') else {
            continue;
        };
        if !raw.starts_with('[') {
            continue;
        }

        let timestamp = &raw[1..close];
        let text = raw[close + 1..].trim().to_string();

        if let Some(ms) = parse_timestamp(timestamp) {
            lines.push(LyricLine { time_ms: ms, text });
        }
    }

    lines.sort_by_key(|l| l.time_ms);
    lines
}

/// Parse `MM:SS.xx` into milliseconds.
fn parse_timestamp(ts: &str) -> Option<i64> {
    let (min_part, rest) = ts.split_once(':')?;
    let (sec_part, frac_part) = rest.split_once('.')?;

    let minutes: i64 = min_part.parse().ok()?;
    let seconds: i64 = sec_part.parse().ok()?;

    // Fractional part: 2 digits = centiseconds, 3 digits = milliseconds.
    let frac_str = frac_part.trim();
    let millis_frac: i64 = match frac_str.len() {
        1 => frac_str.parse::<i64>().ok()? * 100,
        2 => frac_str.parse::<i64>().ok()? * 10,
        3 => frac_str.parse::<i64>().ok()?,
        _ => return None,
    };

    Some(minutes * 60_000 + seconds * 1_000 + millis_frac)
}

// ---------------------------------------------------------------------------
// LRCLIB fetch
// ---------------------------------------------------------------------------

/// Fetch lyrics from LRCLIB for a given artist/track/duration.
///
/// `duration_secs` is the track length in seconds (integer). LRCLIB
/// uses it for disambiguation when multiple versions exist.
pub async fn fetch_from_lrclib(
    client: &reqwest::Client,
    artist: &str,
    track_name: &str,
    duration_secs: Option<i64>,
) -> Result<Lyrics, String> {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(track_name),
    );

    if let Some(dur) = duration_secs {
        url.push_str(&format!("&duration={dur}"));
    }

    debug!(url = %url, "lrclib_fetch");

    let resp = client
        .get(&url)
        .header("User-Agent", "Tune Music Server (https://mozaiklabs.fr)")
        .send()
        .await
        .map_err(|e| format!("lrclib request failed: {e}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Lyrics {
            synced: false,
            lines: Vec::new(),
            plain_text: None,
            source: "lrclib".into(),
        });
    }

    if !resp.status().is_success() {
        return Err(format!("lrclib returned {}", resp.status()));
    }

    let body: LrclibResponse = resp
        .json()
        .await
        .map_err(|e| format!("lrclib parse error: {e}"))?;

    let lines = body
        .synced_lyrics
        .as_deref()
        .map(parse_lrc)
        .unwrap_or_default();

    Ok(Lyrics {
        synced: !lines.is_empty(),
        lines,
        plain_text: body.plain_lyrics,
        source: "lrclib".into(),
    })
}

// ---------------------------------------------------------------------------
// Cache layer
// ---------------------------------------------------------------------------

/// Load cached lyrics from the `lyrics_cache` table.
fn load_cached(db: &Arc<dyn DbBackend>, track_id: i64) -> Option<Lyrics> {
    let sql = "SELECT synced_lyrics, plain_lyrics, source FROM lyrics_cache WHERE track_id = ?";
    let params: [&dyn ToSqlValue; 1] = [&track_id];

    let row = db.query_one(sql, &params).ok()??;
    if row.len() < 3 {
        return None;
    }

    let synced_raw = row[0].as_str().map(|s| s.to_string());
    let plain = row[1].as_str().map(|s| s.to_string());
    let source = row[2].as_str().unwrap_or("lrclib").to_string();

    let lines = synced_raw.as_deref().map(parse_lrc).unwrap_or_default();

    Some(Lyrics {
        synced: !lines.is_empty(),
        lines,
        plain_text: plain,
        source,
    })
}

/// Store lyrics in the `lyrics_cache` table (upsert).
fn store_cache(db: &Arc<dyn DbBackend>, track_id: i64, title: &str, artist: &str, lyrics: &Lyrics) {
    let synced_text: Option<String> = if lyrics.synced {
        // Re-serialize LRC lines back to canonical LRC text.
        Some(
            lyrics
                .lines
                .iter()
                .map(|l| {
                    let mins = l.time_ms / 60_000;
                    let secs = (l.time_ms % 60_000) / 1_000;
                    let centis = (l.time_ms % 1_000) / 10;
                    format!("[{mins:02}:{secs:02}.{centis:02}] {}", l.text)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        None
    };

    let sql = "INSERT OR REPLACE INTO lyrics_cache \
               (track_id, title, artist, synced_lyrics, plain_lyrics, source, fetched_at) \
               VALUES (?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))";

    let params: [&dyn ToSqlValue; 6] = [
        &track_id,
        &title,
        &artist,
        &synced_text.as_deref() as &dyn ToSqlValue,
        &lyrics.plain_text.as_deref() as &dyn ToSqlValue,
        &lyrics.source.as_str(),
    ];

    if let Err(e) = db.execute(sql, &params) {
        warn!(error = %e, track_id, "lyrics_cache_store_failed");
    }
}

// ---------------------------------------------------------------------------
// Public API: cache-first, fallback to LRCLIB
// ---------------------------------------------------------------------------

/// Get lyrics for a track. Checks the DB cache first, then falls back
/// to LRCLIB if not cached.
pub async fn get_lyrics(
    db: &Arc<dyn DbBackend>,
    client: &reqwest::Client,
    track_id: i64,
    title: &str,
    artist: &str,
    duration_ms: i64,
) -> Result<Lyrics, String> {
    // 1. Try cache.
    if let Some(cached) = load_cached(db, track_id) {
        debug!(track_id, "lyrics_cache_hit");
        return Ok(cached);
    }

    // 2. Fetch from LRCLIB.
    let duration_secs = if duration_ms > 0 {
        Some(duration_ms / 1000)
    } else {
        None
    };

    let lyrics = fetch_from_lrclib(client, artist, title, duration_secs).await?;

    // 3. Cache result (even empty — avoids repeated failed lookups).
    store_cache(db, track_id, title, artist, &lyrics);

    Ok(lyrics)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lrc_basic() {
        let lrc = "\
[00:12.34] First line
[00:15.00] Second line
[01:02.50] Third line
";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 12_340);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].time_ms, 15_000);
        assert_eq!(lines[2].time_ms, 62_500);
        assert_eq!(lines[2].text, "Third line");
    }

    #[test]
    fn parse_lrc_three_digit_frac() {
        let lrc = "[00:05.123] Precise";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 5_123);
    }

    #[test]
    fn parse_lrc_empty_and_garbage() {
        let lrc = "\n\nnot a timestamp\n[bad] nope\n";
        let lines = parse_lrc(lrc);
        assert!(lines.is_empty());
    }

    #[test]
    fn parse_lrc_single_digit_frac() {
        let lrc = "[00:03.5] Single";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 3_500);
    }
}
