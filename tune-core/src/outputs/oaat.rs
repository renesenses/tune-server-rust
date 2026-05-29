use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

pub struct OaatOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    current_uri: Arc<Mutex<Option<String>>>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
}

impl OaatOutput {
    pub fn new(name: String, host: String, port: u16, endpoint_id: String) -> Self {
        let device_id = format!("oaat:{endpoint_id}");
        Self {
            name,
            device_id,
            host,
            port,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(800)),
            current_uri: Arc::new(Mutex::new(None)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
        }
    }

    pub fn endpoint_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[async_trait::async_trait]
impl OutputTarget for OaatOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "oaat"
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.stop().await.ok();

        info!(
            device = %self.name,
            url = media.url,
            title = media.title.unwrap_or("-"),
            "oaat: play_media"
        );

        *self.current_uri.lock().await = Some(media.url.to_owned());
        *self.current_title.lock().await = media.title.map(|s| s.to_owned());
        *self.current_artist.lock().await = media.artist.map(|s| s.to_owned());

        // TODO: connect to OAAT endpoint, handshake, propose format,
        // fetch audio from media.url, decode, and stream via UDP.
        // For now, this is a scaffold that will be wired once oaat-controller
        // is integrated as a dependency.

        self.playing.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);

        debug!(
            endpoint = %self.endpoint_addr(),
            "oaat: would stream to endpoint (scaffold)"
        );

        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        info!(device = %self.name, "oaat: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, "oaat: resume");
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        *self.current_uri.lock().await = None;
        info!(device = %self.name, "oaat: stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        debug!(device = %self.name, position_ms, "oaat: seek (not yet implemented)");
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.volume
            .store((volume.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::SeqCst);
        debug!(device = %self.name, volume, "oaat: volume");
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted {
            self.volume.store(0, Ordering::SeqCst);
        }
        debug!(device = %self.name, muted, "oaat: mute");
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let state = if self.playing.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                TransportState::Paused
            } else {
                TransportState::Playing
            }
        } else {
            TransportState::Stopped
        };

        Ok(OutputStatus {
            state,
            position_ms: 0,
            duration_ms: 0,
            volume: self.volume.load(Ordering::Relaxed) as f64 / 1000.0,
            muted: self.volume.load(Ordering::Relaxed) == 0,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
        })
    }

    async fn is_available(&self) -> bool {
        // TODO: TCP probe or cached mDNS state
        true
    }
}
