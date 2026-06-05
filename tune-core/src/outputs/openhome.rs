use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Client;
use tracing::{debug, info, warn};

use super::didl::DidlBuilder;
use super::oh_events::{EventState, OpenHomeEventListener};
use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

const SOAP_MAX_RETRIES: usize = 2;

const SVC_PRODUCT: &str = "urn:av-openhome-org:service:Product:1";
const SVC_PLAYLIST: &str = "urn:av-openhome-org:service:Playlist:1";
const SVC_TRANSPORT: &str = "urn:av-openhome-org:service:Transport:1";
const SVC_VOLUME: &str = "urn:av-openhome-org:service:Volume:1";
const SVC_INFO: &str = "urn:av-openhome-org:service:Info:1";
const SVC_TIME: &str = "urn:av-openhome-org:service:Time:1";

pub struct OpenHomeOutput {
    name: String,
    device_id: String,
    host_addr: String,
    service_urls: HashMap<String, String>,
    event_sub_urls: HashMap<String, String>,
    event_listener: Option<Arc<OpenHomeEventListener>>,
    event_state: Arc<tokio::sync::Mutex<EventState>>,
    event_sub_ids: tokio::sync::Mutex<Vec<String>>,
    client: Client,
    current_oh_id: tokio::sync::Mutex<Option<u32>>,
}

impl OpenHomeOutput {
    pub fn new(
        name: String,
        device_id: String,
        host: String,
        port: u16,
        service_paths: HashMap<String, String>,
        event_listener: Option<Arc<OpenHomeEventListener>>,
        event_sub_paths: HashMap<String, String>,
    ) -> Self {
        let base = format!("http://{}:{}", host, port);
        let service_urls = service_paths
            .into_iter()
            .map(|(k, path)| (k, format!("{}{}", base, path)))
            .collect();
        let event_sub_urls = event_sub_paths
            .into_iter()
            .map(|(k, path)| (k, format!("{}{}", base, path)))
            .collect();

        Self {
            name,
            device_id,
            host_addr: host,
            service_urls,
            event_sub_urls,
            event_listener,
            event_state: Arc::new(tokio::sync::Mutex::new(EventState::default())),
            event_sub_ids: tokio::sync::Mutex::new(Vec::new()),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            current_oh_id: tokio::sync::Mutex::new(None),
        }
    }

    fn svc_url(&self, key: &str) -> Option<&String> {
        self.service_urls.get(key)
    }

    async fn soap_call(
        &self,
        url: &str,
        service_type: &str,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<String, String> {
        let mut body_args = String::new();
        for (k, v) in args {
            let escaped = quick_xml::escape::escape(*v);
            body_args.push_str(&format!("<{k}>{escaped}</{k}>"));
        }

        let envelope = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service_type}">
      {body_args}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );

        let soap_action = format!("{service_type}#{action}");
        let mut last_err = String::new();

        for attempt in 0..=SOAP_MAX_RETRIES {
            if attempt > 0 {
                let delay = 200 * (1 << (attempt - 1));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                debug!(device = %self.name, action, attempt, "oh_soap_retry");
            }

            match self
                .client
                .post(url)
                .header("Content-Type", r#"text/xml; charset="utf-8""#)
                .header("SOAPAction", format!("\"{soap_action}\""))
                .body(envelope.clone())
                .send()
                .await
            {
                Ok(resp) => match resp.text().await {
                    Ok(text) => return Ok(text),
                    Err(e) => last_err = format!("oh read: {e}"),
                },
                Err(e) if e.is_connect() || e.is_timeout() => {
                    last_err = format!("oh send: {e}");
                }
                Err(e) => return Err(format!("oh send: {e}")),
            }
        }

        warn!(device = %self.name, action, error = %last_err, "oh_soap_all_retries_failed");
        Err(last_err)
    }

    async fn oh_play(&self) -> Result<(), String> {
        if let Some(url) = self.svc_url("transport") {
            self.soap_call(url, SVC_TRANSPORT, "Play", &[]).await?;
        } else if let Some(url) = self.svc_url("playlist") {
            self.soap_call(url, SVC_PLAYLIST, "Play", &[]).await?;
        }
        Ok(())
    }

    async fn oh_pause(&self) -> Result<(), String> {
        if let Some(url) = self.svc_url("transport") {
            self.soap_call(url, SVC_TRANSPORT, "Pause", &[]).await?;
        } else if let Some(url) = self.svc_url("playlist") {
            self.soap_call(url, SVC_PLAYLIST, "Pause", &[]).await?;
        }
        Ok(())
    }

    async fn oh_stop(&self) -> Result<(), String> {
        if let Some(url) = self.svc_url("transport") {
            self.soap_call(url, SVC_TRANSPORT, "Stop", &[]).await?;
        } else if let Some(url) = self.svc_url("playlist") {
            self.soap_call(url, SVC_PLAYLIST, "Stop", &[]).await?;
        }
        Ok(())
    }

    async fn transport_state(&self) -> TransportState {
        let Some(url) = self.svc_url("transport") else {
            return TransportState::Stopped;
        };
        let Ok(resp) = self
            .soap_call(url, SVC_TRANSPORT, "TransportState", &[])
            .await
        else {
            return TransportState::Stopped;
        };
        match extract_tag(&resp, "State").as_deref() {
            Some("Playing") => TransportState::Playing,
            Some("Paused") => TransportState::Paused,
            Some("Buffering") => TransportState::Transitioning,
            _ => TransportState::Stopped,
        }
    }

    async fn playlist_delete_all(&self) -> Result<(), String> {
        if let Some(url) = self.svc_url("playlist") {
            self.soap_call(url, SVC_PLAYLIST, "DeleteAll", &[]).await?;
        }
        Ok(())
    }

    async fn playlist_insert(
        &self,
        after_id: u32,
        uri: &str,
        metadata: &str,
    ) -> Result<Option<u32>, String> {
        let Some(url) = self.svc_url("playlist") else {
            return Err("no playlist service".into());
        };
        let after = after_id.to_string();
        let resp = self
            .soap_call(
                url,
                SVC_PLAYLIST,
                "Insert",
                &[("AfterId", &after), ("Uri", uri), ("Metadata", metadata)],
            )
            .await?;
        Ok(extract_tag(&resp, "NewId").and_then(|v| v.parse().ok()))
    }

    async fn playlist_seek_id(&self, id: u32) -> Result<(), String> {
        if let Some(url) = self.svc_url("playlist") {
            self.soap_call(url, SVC_PLAYLIST, "SeekId", &[("Value", &id.to_string())])
                .await?;
        }
        Ok(())
    }

    async fn select_playlist_source(&self) {
        let Some(url) = self.svc_url("product") else {
            return;
        };
        let Ok(resp) = self.soap_call(url, SVC_PRODUCT, "SourceXml", &[]).await else {
            return;
        };
        let Some(xml) = extract_tag(&resp, "Value") else {
            return;
        };

        let mut idx = 0u32;
        for chunk in xml.split("<Source>").skip(1) {
            if let Some(stype) = extract_tag(chunk, "Type")
                && stype.trim() == "Playlist"
            {
                if let Err(e) = self
                    .soap_call(
                        url,
                        SVC_PRODUCT,
                        "SetSourceIndex",
                        &[("Value", &idx.to_string())],
                    )
                    .await
                {
                    warn!(device = %self.name, error = %e, "oh_source_select_failed");
                } else {
                    debug!(device = %self.name, index = idx, "oh_source_set_playlist");
                }
                return;
            }
            idx += 1;
        }
    }

    async fn wake_from_standby(&self) {
        let Some(url) = self.svc_url("product") else {
            return;
        };
        let Ok(resp) = self.soap_call(url, SVC_PRODUCT, "Standby", &[]).await else {
            return;
        };
        let is_standby = extract_tag(&resp, "Value")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if is_standby {
            let _ = self
                .soap_call(url, SVC_PRODUCT, "SetStandby", &[("Value", "0")])
                .await;
            info!(device = %self.name, "oh_woke_from_standby");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    async fn subscribe_events(&self) {
        let Some(listener) = &self.event_listener else {
            return;
        };
        let state = self.event_state.clone();
        let services = ["transport", "info", "volume", "time", "playlist"];
        let mut sub_ids = self.event_sub_ids.lock().await;

        let mut count = 0u32;
        for svc in &services {
            if let Some(url) = self.event_sub_urls.get(*svc)
                && let Some(path_id) = listener.subscribe(url, state.clone()).await
            {
                sub_ids.push(path_id);
                count += 1;
            }
        }

        if count > 0 {
            info!(device = %self.name, count, "oh_events_subscribed");
        }
    }

    async fn unsubscribe_events(&self) {
        let Some(listener) = &self.event_listener else {
            return;
        };
        let mut sub_ids = self.event_sub_ids.lock().await;
        for path_id in sub_ids.drain(..) {
            listener.unsubscribe(&path_id).await;
        }
    }

    fn build_didl(
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        mime_type: &str,
        url: &str,
        cover_url: Option<&str>,
    ) -> String {
        DidlBuilder::new(title.unwrap_or("Unknown"), url, mime_type)
            .artist(artist.unwrap_or("Unknown"))
            .album_opt(album)
            .album_art_opt(cover_url)
            .include_upnp_artist(true)
            .build()
    }
}

#[async_trait::async_trait]
impl OutputTarget for OpenHomeOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "openhome"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host_addr)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.unsubscribe_events().await;
        self.oh_stop().await.ok();
        self.wake_from_standby().await;
        self.select_playlist_source().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let metadata = Self::build_didl(
            media.title,
            media.artist,
            media.album,
            media.mime_type,
            media.url,
            media.cover_url,
        );

        self.playlist_delete_all().await?;

        let new_id = self.playlist_insert(0, media.url, &metadata).await?;
        if let Some(id) = new_id {
            *self.current_oh_id.lock().await = Some(id);
            self.playlist_seek_id(id).await?;
            self.oh_play().await?;
            self.subscribe_events().await;
            info!(device = %self.name, url = media.url, oh_id = id, "oh_play");
            Ok(())
        } else {
            Err("playlist insert returned no ID".into())
        }
    }

    async fn pause(&self) -> Result<(), String> {
        self.oh_pause().await
    }

    async fn resume(&self) -> Result<(), String> {
        self.oh_play().await
    }

    async fn stop(&self) -> Result<(), String> {
        self.unsubscribe_events().await;
        self.oh_stop().await?;
        *self.current_oh_id.lock().await = None;
        info!(device = %self.name, "oh_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let seconds = (position_ms / 1000).to_string();
        if let Some(url) = self.svc_url("transport") {
            self.soap_call(
                url,
                SVC_TRANSPORT,
                "SeekSecondAbsolute",
                &[("Value", &seconds)],
            )
            .await?;
        } else if let Some(url) = self.svc_url("playlist") {
            self.soap_call(
                url,
                SVC_PLAYLIST,
                "SeekSecondAbsolute",
                &[("Value", &seconds)],
            )
            .await?;
        }
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let level = (volume * 100.0).round().clamp(0.0, 100.0) as u32;
        if let Some(url) = self.svc_url("volume") {
            self.soap_call(
                url,
                SVC_VOLUME,
                "SetVolume",
                &[("Value", &level.to_string())],
            )
            .await?;
        }
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { "1" } else { "0" };
        if let Some(url) = self.svc_url("volume") {
            self.soap_call(url, SVC_VOLUME, "SetMute", &[("Value", val)])
                .await?;
        }
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        // Fast path: use cached event state for transport/volume/mute
        let es = self.event_state.lock().await;
        let eventing = es.is_fresh();
        let cached_state = es.transport_state;
        let cached_volume = es.volume.map(|v| v as f64 / 100.0);
        let cached_muted = es.muted;
        let cached_uri = es.track_uri.clone();
        drop(es);

        let state = if let Some(s) = cached_state.filter(|_| eventing) {
            s
        } else {
            self.transport_state().await
        };

        // Position always via SOAP (events don't reliably push time)
        let (position_ms, duration_ms) = if let Some(url) = self.svc_url("time") {
            if let Ok(resp) = self.soap_call(url, SVC_TIME, "Time", &[]).await {
                let dur = extract_tag(&resp, "TrackDuration")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 1000;
                let pos = extract_tag(&resp, "Seconds")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 1000;
                (pos, dur)
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };

        let volume = if let Some(v) = cached_volume.filter(|_| eventing) {
            v
        } else if let Some(url) = self.svc_url("volume") {
            self.soap_call(url, SVC_VOLUME, "Volume", &[])
                .await
                .ok()
                .and_then(|r| extract_tag(&r, "Value"))
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| v / 100.0)
                .unwrap_or(0.5)
        } else {
            0.5
        };

        let muted = if let Some(m) = cached_muted.filter(|_| eventing) {
            m
        } else if let Some(url) = self.svc_url("volume") {
            self.soap_call(url, SVC_VOLUME, "Mute", &[])
                .await
                .ok()
                .and_then(|r| extract_tag(&r, "Value"))
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        } else {
            false
        };

        let current_uri = if eventing { cached_uri } else { None };

        let (current_uri, track_title, track_artist) = if current_uri.is_some() {
            (current_uri, None, None)
        } else if let Some(url) = self.svc_url("info") {
            if let Ok(resp) = self.soap_call(url, SVC_INFO, "Track", &[]).await {
                let uri = extract_tag(&resp, "Uri");
                let metadata = extract_tag(&resp, "Metadata").unwrap_or_default();
                let title = extract_tag(&metadata, "dc:title");
                let artist = extract_tag(&metadata, "dc:creator")
                    .or_else(|| extract_tag(&metadata, "upnp:artist"));
                (uri, title, artist)
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted,
            current_uri,
            track_title,
            track_artist,
        })
    }

    async fn is_available(&self) -> bool {
        if let Some(url) = self.svc_url("product") {
            self.soap_call(url, SVC_PRODUCT, "Standby", &[])
                .await
                .is_ok()
        } else if let Some(url) = self.svc_url("playlist") {
            self.soap_call(url, SVC_PLAYLIST, "IdArray", &[])
                .await
                .is_ok()
        } else {
            false
        }
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let metadata = Self::build_didl(
            media.title,
            media.artist,
            media.album,
            media.mime_type,
            media.url,
            media.cover_url,
        );
        let after_id = self.current_oh_id.lock().await.unwrap_or(0);
        let new_id = self.playlist_insert(after_id, media.url, &metadata).await?;
        if let Some(id) = new_id {
            info!(device = %self.name, url = media.url, oh_id = id, "oh_set_next");
        }
        Ok(())
    }
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let text = xml[start..end].trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn didl_with_all_fields() {
        let didl = OpenHomeOutput::build_didl(
            Some("Test Track"),
            Some("Test Artist"),
            Some("Test Album"),
            "audio/flac",
            "http://example.com/stream",
            Some("http://example.com/cover.jpg"),
        );
        assert!(didl.contains("Test Track"));
        assert!(didl.contains("Test Artist"));
        assert!(didl.contains("Test Album"));
        assert!(didl.contains("albumArtURI"));
        assert!(didl.contains("cover.jpg"));
        assert!(didl.contains("DIDL-Lite"));
    }

    #[test]
    fn didl_without_optional_fields() {
        let didl = OpenHomeOutput::build_didl(
            None,
            None,
            None,
            "audio/flac",
            "http://example.com/stream",
            None,
        );
        assert!(didl.contains("Unknown"));
        assert!(!didl.contains("albumArtURI"));
        assert!(!didl.contains("upnp:album"));
    }

    #[test]
    fn didl_escapes_special_chars() {
        let didl = OpenHomeOutput::build_didl(
            Some("Rock & Roll"),
            Some("AC/DC"),
            None,
            "audio/flac",
            "http://example.com/stream?a=1&b=2",
            None,
        );
        assert!(didl.contains("Rock &amp; Roll"));
        assert!(didl.contains("a=1&amp;b=2"));
    }

    #[test]
    fn extract_tag_works() {
        let xml = "<State>Playing</State><Value>42</Value>";
        assert_eq!(extract_tag(xml, "State"), Some("Playing".into()));
        assert_eq!(extract_tag(xml, "Value"), Some("42".into()));
        assert_eq!(extract_tag(xml, "Missing"), None);
    }

    #[test]
    fn extract_tag_empty() {
        let xml = "<Value></Value>";
        assert_eq!(extract_tag(xml, "Value"), None);
    }
}
