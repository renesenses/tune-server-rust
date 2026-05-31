use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::playback::{NowPlaying, PlaybackManager};

pub struct RadioMetadataHandler {
    playback: Arc<PlaybackManager>,
    client: Client,
    polling: Mutex<bool>,
    cancel: Mutex<bool>,
}

impl RadioMetadataHandler {
    pub fn new(playback: Arc<PlaybackManager>) -> Self {
        Self {
            playback,
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            polling: Mutex::new(false),
            cancel: Mutex::new(false),
        }
    }

    pub async fn start_icy_polling(
        self: Arc<Self>,
        zone_id: i64,
        stream_url: String,
        station_name: String,
        station_cover: Option<String>,
        interval: Duration,
    ) {
        *self.cancel.lock().await = false;
        *self.polling.lock().await = true;

        let handler = self.clone();
        tokio::spawn(async move {
            info!(zone_id, url = %stream_url, "icy_polling_start");
            loop {
                if *handler.cancel.lock().await {
                    break;
                }

                match handler.fetch_icy_metadata(&stream_url).await {
                    Ok(Some(meta)) => {
                        let title = meta.title.unwrap_or_default();
                        let artist = meta.artist.clone();

                        if !title.is_empty() {
                            let (track_title, track_artist) = if let Some(ref a) = artist {
                                (title.clone(), Some(a.clone()))
                            } else {
                                parse_icy_title(&title, &station_name)
                            };

                            let np = NowPlaying {
                                track_id: None,
                                title: track_title,
                                artist_name: track_artist,
                                album_title: Some(station_name.clone()),
                                cover_path: meta.cover_url.or_else(|| station_cover.clone()),
                                duration_ms: 0,
                                source: "radio".into(),
                                source_id: None,
                                stream_id: None,
                            };

                            handler.playback.update_now_playing(zone_id, np).await;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(error = %e, "icy_fetch_error");
                    }
                }

                tokio::time::sleep(interval).await;
            }
            *handler.polling.lock().await = false;
            debug!(zone_id, "icy_polling_stopped");
        });
    }

    pub async fn stop(&self) {
        *self.cancel.lock().await = true;
    }

    pub async fn is_polling(&self) -> bool {
        *self.polling.lock().await
    }

    async fn fetch_icy_metadata(&self, url: &str) -> Result<Option<IcyMetadata>, String> {
        let resp = self
            .client
            .get(url)
            .header("Icy-MetaData", "1")
            .send()
            .await
            .map_err(|e| format!("icy request: {e}"))?;

        let headers = resp.headers();
        let icy_name = headers
            .get("icy-name")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let icy_title = headers
            .get("icy-title")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let icy_genre = headers
            .get("icy-genre")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        if icy_title.is_some() || icy_name.is_some() {
            return Ok(Some(IcyMetadata {
                title: icy_title,
                artist: None,
                station_name: icy_name,
                genre: icy_genre,
                cover_url: None,
            }));
        }

        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct IcyMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub station_name: Option<String>,
    pub genre: Option<String>,
    pub cover_url: Option<String>,
}

fn parse_icy_title(raw: &str, station_name: &str) -> (String, Option<String>) {
    if let Some((artist, title)) = raw.split_once(" - ") {
        let artist = artist.trim();
        let title = title.trim();
        if !artist.is_empty() && !title.is_empty() {
            return (title.to_string(), Some(artist.to_string()));
        }
    }

    if let Some((artist, title)) = raw.split_once(" / ") {
        let artist = artist.trim();
        let title = title.trim();
        if !artist.is_empty() && !title.is_empty() {
            return (title.to_string(), Some(artist.to_string()));
        }
    }

    (raw.to_string(), Some(station_name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_artist_dash_title() {
        let (title, artist) = parse_icy_title("Queen - Bohemian Rhapsody", "Radio");
        assert_eq!(title, "Bohemian Rhapsody");
        assert_eq!(artist.as_deref(), Some("Queen"));
    }

    #[test]
    fn parse_artist_slash_title() {
        let (title, artist) = parse_icy_title("Daft Punk / Get Lucky", "FIP");
        assert_eq!(title, "Get Lucky");
        assert_eq!(artist.as_deref(), Some("Daft Punk"));
    }

    #[test]
    fn parse_no_separator() {
        let (title, artist) = parse_icy_title("Just a title", "Station FM");
        assert_eq!(title, "Just a title");
        assert_eq!(artist.as_deref(), Some("Station FM"));
    }

    #[test]
    fn parse_empty() {
        let (title, artist) = parse_icy_title("", "Radio");
        assert_eq!(title, "");
        assert_eq!(artist.as_deref(), Some("Radio"));
    }
}
