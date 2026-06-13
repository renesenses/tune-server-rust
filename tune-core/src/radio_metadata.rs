use serde::{Deserialize, Serialize};
use tracing::debug;

/// Metadata extracted from a radio stream (ICY or external API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IcyMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub station: Option<String>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Fetch metadata for the given radio station.
///
/// The function first checks whether the station URL matches a known metadata
/// API (Radio France / FIP, Radio Paradise) and uses those richer endpoints.
/// As a fallback it attempts to read raw ICY metadata from the audio stream.
pub async fn fetch_radio_metadata(station_name: &str, stream_url: &str) -> Option<IcyMetadata> {
    // Radio France family (FIP, France Inter, France Musique, ...)
    if stream_url.contains("fipradio")
        || stream_url.contains("radiofrance")
        || stream_url.contains("fip-")
        || station_name.to_lowercase().contains("fip")
        || station_name.to_lowercase().contains("france musique")
        || station_name.to_lowercase().contains("france inter")
    {
        let channel = radiofrance_channel_id(station_name, stream_url);
        return fetch_radiofrance_metadata(station_name, channel).await;
    }

    // Radio Paradise
    if stream_url.contains("radioparadise")
        || station_name.to_lowercase().contains("radio paradise")
    {
        let chan = radioparadise_channel(stream_url);
        return fetch_radio_paradise_metadata(station_name, chan).await;
    }

    // Fallback: raw ICY metadata
    fetch_icy_metadata(stream_url).await
}

// ---------------------------------------------------------------------------
// Radio France
// ---------------------------------------------------------------------------

/// Map a station name / stream URL to the Radio France *station id* used by
/// their live-meta API.  Default = 7 (FIP).
fn radiofrance_channel_id(_station_name: &str, stream_url: &str) -> u32 {
    if stream_url.contains("franceinter") {
        1
    } else if stream_url.contains("francemusique") || stream_url.contains("france-musique") {
        4
    } else if stream_url.contains("mouv") {
        6
    } else if stream_url.contains("fip") {
        7
    } else if stream_url.contains("franceculture") || stream_url.contains("france-culture") {
        2
    } else if stream_url.contains("franceinfo") {
        3
    } else {
        7 // default to FIP
    }
}

async fn fetch_radiofrance_metadata(station_name: &str, channel: u32) -> Option<IcyMetadata> {
    let url = format!("https://api.radiofrance.fr/livemeta/pull/{channel}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "radiofrance_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;

    // New API format: levels[0].items[position] → step UUID → steps[uuid]
    let levels = body.get("levels")?.as_array()?;
    let level = levels.first()?;
    let position = level.get("position")?.as_u64()? as usize;
    let items = level.get("items")?.as_array()?;
    let current_id = items.get(position)?.as_str()?;
    let steps = body.get("steps")?.as_object()?;
    let now = steps.get(current_id)?;

    let title = now
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if title.is_empty() {
        return None;
    }

    let artist = now
        .get("authors")
        .and_then(|v| v.as_str())
        .or_else(|| {
            now.get("song")
                .and_then(|s| s.get("interpreters"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_string());

    Some(IcyMetadata {
        title,
        artist,
        station: Some(station_name.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Radio Paradise
// ---------------------------------------------------------------------------

fn radioparadise_channel(stream_url: &str) -> u32 {
    if stream_url.contains("chan=1") || stream_url.contains("mellow") {
        1
    } else if stream_url.contains("chan=2") || stream_url.contains("rock") {
        2
    } else if stream_url.contains("chan=3") || stream_url.contains("world") {
        3
    } else {
        0 // main mix
    }
}

async fn fetch_radio_paradise_metadata(station_name: &str, chan: u32) -> Option<IcyMetadata> {
    let url = format!("https://api.radioparadise.com/api/now_playing?chan={chan}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        debug!(station = %station_name, status = %resp.status(), "radioparadise_api_error");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let artist = body
        .get("artist")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if title.is_empty() {
        return None;
    }

    Some(IcyMetadata {
        title,
        artist,
        station: Some(station_name.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Raw ICY metadata (fallback)
// ---------------------------------------------------------------------------

async fn fetch_icy_metadata(stream_url: &str) -> Option<IcyMetadata> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get(stream_url)
        .header("Icy-MetaData", "1")
        .send()
        .await
        .ok()?;

    let icy_metaint: usize = resp
        .headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())?;

    let station = resp
        .headers()
        .get("icy-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // We need to read `icy_metaint` bytes of audio, then one length byte,
    // then `length * 16` bytes of metadata text.
    let bytes = resp.bytes().await.ok()?;

    if bytes.len() <= icy_metaint {
        return None;
    }

    let meta_length_byte = bytes[icy_metaint] as usize;
    if meta_length_byte == 0 {
        return None;
    }

    let meta_len = meta_length_byte * 16;
    let meta_start = icy_metaint + 1;
    let meta_end = meta_start + meta_len;
    if bytes.len() < meta_end {
        return None;
    }

    let raw = String::from_utf8_lossy(&bytes[meta_start..meta_end]);
    parse_icy_string(&raw, station)
}

/// Parse the ICY metadata string.
///
/// Typical format: `StreamTitle='Artist - Title';StreamUrl='';`
fn parse_icy_string(raw: &str, station: Option<String>) -> Option<IcyMetadata> {
    let trimmed = raw.trim_end_matches('\0');

    // Extract StreamTitle value
    let title_start = trimmed.find("StreamTitle='")?;
    let after = &trimmed[title_start + "StreamTitle='".len()..];
    let title_end = after.find("';")?;
    let stream_title = &after[..title_end];

    if stream_title.is_empty() {
        return None;
    }

    // Try to split "Artist - Title"
    let (artist, title) = if let Some(sep) = stream_title.find(" - ") {
        (
            Some(stream_title[..sep].to_string()),
            stream_title[sep + 3..].to_string(),
        )
    } else {
        (None, stream_title.to_string())
    };

    Some(IcyMetadata {
        title,
        artist,
        station,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_icy_artist_title() {
        let raw = "StreamTitle='Miles Davis - So What';StreamUrl='';";
        let meta = parse_icy_string(raw, Some("Jazz FM".into())).unwrap();
        assert_eq!(meta.title, "So What");
        assert_eq!(meta.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(meta.station.as_deref(), Some("Jazz FM"));
    }

    #[test]
    fn parse_icy_title_only() {
        let raw = "StreamTitle='Unknown Show';StreamUrl='';";
        let meta = parse_icy_string(raw, None).unwrap();
        assert_eq!(meta.title, "Unknown Show");
        assert!(meta.artist.is_none());
    }

    #[test]
    fn parse_icy_empty() {
        let raw = "StreamTitle='';StreamUrl='';";
        assert!(parse_icy_string(raw, None).is_none());
    }

    #[test]
    fn parse_icy_with_null_padding() {
        let mut raw = String::from("StreamTitle='FIP - Jazz';StreamUrl='';");
        raw.push_str("\0\0\0\0\0");
        let meta = parse_icy_string(&raw, None).unwrap();
        assert_eq!(meta.title, "Jazz");
        assert_eq!(meta.artist.as_deref(), Some("FIP"));
    }

    #[test]
    fn radiofrance_channel_detection() {
        assert_eq!(
            radiofrance_channel_id("FIP", "https://icecast.radiofrance.fr/fip-hifi.aac"),
            7
        );
        assert_eq!(
            radiofrance_channel_id(
                "Inter",
                "https://icecast.radiofrance.fr/franceinter-hifi.aac"
            ),
            1
        );
        assert_eq!(
            radiofrance_channel_id(
                "Musique",
                "https://icecast.radiofrance.fr/francemusique-hifi.aac"
            ),
            4
        );
        assert_eq!(
            radiofrance_channel_id("Mouv", "https://icecast.radiofrance.fr/mouv-hifi.aac"),
            6
        );
    }

    #[test]
    fn radioparadise_channel_detection() {
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/aac-320"),
            0
        );
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/mellow-320"),
            1
        );
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/rock-320"),
            2
        );
        assert_eq!(
            radioparadise_channel("http://stream.radioparadise.com/world-320"),
            3
        );
    }
}
