use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::traits::TransportState;

const SUBSCRIBE_TIMEOUT: &str = "Second-300";
const RENEW_INTERVAL_SECS: u64 = 250;
const EVENT_STALE_SECS: u64 = 10;

static SUB_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_path_id() -> String {
    format!("oh{}", SUB_COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug, Default)]
pub struct EventState {
    pub transport_state: Option<TransportState>,
    pub volume: Option<u32>,
    pub muted: Option<bool>,
    pub track_uri: Option<String>,
    pub last_update: Option<Instant>,
}

impl EventState {
    pub fn is_fresh(&self) -> bool {
        self.last_update
            .map(|t| t.elapsed().as_secs() < EVENT_STALE_SECS)
            .unwrap_or(false)
    }

    fn apply_properties(&mut self, props: &HashMap<String, String>) {
        if let Some(state) = props.get("TransportState") {
            self.transport_state = Some(match state.as_str() {
                "Playing" => TransportState::Playing,
                "Paused" => TransportState::Paused,
                "Buffering" => TransportState::Transitioning,
                _ => TransportState::Stopped,
            });
        }

        if let Some(vol) = props.get("Volume").and_then(|v| v.parse().ok()) {
            self.volume = Some(vol);
        }

        if let Some(mute) = props.get("Mute") {
            self.muted = Some(mute == "1" || mute.eq_ignore_ascii_case("true"));
        }

        if let Some(uri) = props.get("Uri")
            && !uri.is_empty()
        {
            self.track_uri = Some(uri.clone());
        }

        self.last_update = Some(Instant::now());
    }
}

pub struct OpenHomeEventListener {
    port: u16,
    server_ip: String,
    client: Client,
    handlers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>>,
    subscriptions: Arc<RwLock<HashMap<String, String>>>,
}

impl OpenHomeEventListener {
    pub async fn new(server_ip: String) -> Result<Self, String> {
        let listener = match TcpListener::bind(("0.0.0.0", 8890)).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind(("0.0.0.0", 0))
                .await
                .map_err(|e| format!("bind oh_events: {e}"))?,
        };
        let port = listener
            .local_addr()
            .map_err(|e| format!("local addr: {e}"))?
            .port();

        let handlers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let subscriptions: Arc<RwLock<HashMap<String, String>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("client: {e}"))?;

        // HTTP NOTIFY receiver
        let h = handlers.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let h = h.clone();
                        tokio::spawn(handle_notify(stream, h));
                    }
                    Err(e) => warn!(error = %e, "oh_event_accept_error"),
                }
            }
        });

        // Subscription renewal loop
        let subs = subscriptions.clone();
        let ip = server_ip.clone();
        let rc = client.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(RENEW_INTERVAL_SECS));
            loop {
                interval.tick().await;
                renew_all(&rc, &subs, &ip, port).await;
            }
        });

        info!(port, "oh_event_listener_started");

        Ok(Self {
            port,
            server_ip,
            client,
            handlers,
            subscriptions,
        })
    }

    fn callback_base(&self) -> String {
        format!("http://{}:{}", self.server_ip, self.port)
    }

    pub async fn subscribe(
        &self,
        event_sub_url: &str,
        state: Arc<tokio::sync::Mutex<EventState>>,
    ) -> Option<String> {
        let path_id = next_path_id();
        let callback_url = format!("{}/oh-event/{}", self.callback_base(), path_id);

        let method = reqwest::Method::from_bytes(b"SUBSCRIBE").ok()?;
        let resp = self
            .client
            .request(method, event_sub_url)
            .header("CALLBACK", format!("<{callback_url}>"))
            .header("NT", "upnp:event")
            .header("TIMEOUT", SUBSCRIBE_TIMEOUT)
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            debug!(url = event_sub_url, status = %resp.status(), "oh_subscribe_rejected");
            return None;
        }

        self.handlers.write().await.insert(path_id.clone(), state);
        self.subscriptions
            .write()
            .await
            .insert(path_id.clone(), event_sub_url.to_string());

        debug!(url = event_sub_url, path_id = %path_id, "oh_subscribed");
        Some(path_id)
    }

    pub async fn unsubscribe(&self, path_id: &str) {
        self.handlers.write().await.remove(path_id);
        if let Some(event_url) = self.subscriptions.write().await.remove(path_id)
            && let Ok(method) = reqwest::Method::from_bytes(b"UNSUBSCRIBE")
        {
            let _ = self
                .client
                .request(method, &event_url)
                .header("SID", path_id)
                .send()
                .await;
        }
    }
}

async fn handle_notify(
    stream: tokio::net::TcpStream,
    handlers: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<EventState>>>>>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    let mut request_line = String::new();
    if buf_reader.read_line(&mut request_line).await.is_err() {
        return;
    }

    let path_id = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.strip_prefix("/oh-event/"))
        .map(|s| s.to_string());

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if buf_reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("content-length:") {
            content_length = line
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 && buf_reader.read_exact(&mut body).await.is_err() {
        let _ = writer.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        return;
    }

    let _ = writer
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await;

    if let Some(path_id) = path_id {
        let body_str = String::from_utf8_lossy(&body);
        let properties = parse_propertyset(&body_str);
        if !properties.is_empty() {
            let handlers = handlers.read().await;
            if let Some(state) = handlers.get(&path_id) {
                state.lock().await.apply_properties(&properties);
                debug!(path_id = %path_id, props = ?properties.keys().collect::<Vec<_>>(), "oh_event_applied");
            }
        }
    }
}

async fn renew_all(
    client: &Client,
    subscriptions: &Arc<RwLock<HashMap<String, String>>>,
    server_ip: &str,
    port: u16,
) {
    let subs = subscriptions.read().await.clone();
    if subs.is_empty() {
        return;
    }
    let Ok(method) = reqwest::Method::from_bytes(b"SUBSCRIBE") else {
        return;
    };
    for (path_id, event_url) in &subs {
        let callback = format!("http://{}:{}/oh-event/{}", server_ip, port, path_id);
        let result = client
            .request(method.clone(), event_url)
            .header("CALLBACK", format!("<{callback}>"))
            .header("NT", "upnp:event")
            .header("TIMEOUT", SUBSCRIBE_TIMEOUT)
            .send()
            .await;
        if let Err(e) = result {
            debug!(url = event_url, error = %e, "oh_renew_failed");
        }
    }
    debug!(count = subs.len(), "oh_subscriptions_renewed");
}

fn parse_propertyset(xml: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_property = false;
    let mut current_tag = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "property" {
                    in_property = true;
                } else if in_property {
                    current_tag = tag;
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "property" {
                    in_property = false;
                    current_tag.clear();
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_property
                    && !current_tag.is_empty()
                    && let Ok(text) = e.unescape()
                {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        result.insert(current_tag.clone(), text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_propertyset_basic() {
        let xml = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <TransportState>Playing</TransportState>
  </e:property>
  <e:property>
    <Volume>42</Volume>
  </e:property>
</e:propertyset>"#;
        let props = parse_propertyset(xml);
        assert_eq!(props.get("TransportState"), Some(&"Playing".to_string()));
        assert_eq!(props.get("Volume"), Some(&"42".to_string()));
    }

    #[test]
    fn parse_propertyset_mute_and_uri() {
        let xml = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property><Mute>true</Mute></e:property>
  <e:property><Uri>http://example.com/track.flac</Uri></e:property>
</e:propertyset>"#;
        let props = parse_propertyset(xml);
        assert_eq!(props.get("Mute"), Some(&"true".to_string()));
        assert_eq!(
            props.get("Uri"),
            Some(&"http://example.com/track.flac".to_string())
        );
    }

    #[test]
    fn parse_propertyset_empty() {
        let props = parse_propertyset("<e:propertyset></e:propertyset>");
        assert!(props.is_empty());
    }

    #[test]
    fn event_state_freshness() {
        let mut state = EventState::default();
        assert!(!state.is_fresh());
        state.last_update = Some(Instant::now());
        assert!(state.is_fresh());
    }

    #[test]
    fn event_state_apply() {
        let mut state = EventState::default();
        let mut props = HashMap::new();
        props.insert("TransportState".to_string(), "Playing".to_string());
        props.insert("Volume".to_string(), "75".to_string());
        props.insert("Mute".to_string(), "0".to_string());
        state.apply_properties(&props);
        assert_eq!(state.transport_state, Some(TransportState::Playing));
        assert_eq!(state.volume, Some(75));
        assert_eq!(state.muted, Some(false));
        assert!(state.is_fresh());
    }

    #[test]
    fn event_state_apply_uri() {
        let mut state = EventState::default();
        let mut props = HashMap::new();
        props.insert("Uri".to_string(), "http://10.0.0.1/stream.flac".to_string());
        state.apply_properties(&props);
        assert_eq!(
            state.track_uri,
            Some("http://10.0.0.1/stream.flac".to_string())
        );
    }
}
