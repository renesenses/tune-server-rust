use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

#[derive(Debug, Clone)]
pub struct PlayCall {
    pub url: String,
    pub title: Option<String>,
}

/// A mock OutputTarget for testing playback flows.
///
/// Tracks all play/stop/pause calls and returns configurable status.
pub struct MockOutput {
    id: String,
    name: String,
    state: Arc<Mutex<TransportState>>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    current_uri: Arc<Mutex<Option<String>>>,
    next_uri: Arc<Mutex<Option<String>>>,
    play_calls: Arc<Mutex<Vec<PlayCall>>>,
    stop_calls: Arc<AtomicU64>,
    set_next_calls: Arc<Mutex<Vec<PlayCall>>>,
}

impl MockOutput {
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            state: Arc::new(Mutex::new(TransportState::Stopped)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(Mutex::new(None)),
            next_uri: Arc::new(Mutex::new(None)),
            play_calls: Arc::new(Mutex::new(Vec::new())),
            stop_calls: Arc::new(AtomicU64::new(0)),
            set_next_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn set_state(&self, state: TransportState) {
        *self.state.lock().await = state;
    }

    pub fn set_position(&self, ms: u64) {
        self.position_ms.store(ms, Ordering::Relaxed);
    }

    pub fn set_duration(&self, ms: u64) {
        self.duration_ms.store(ms, Ordering::Relaxed);
    }

    pub async fn play_call_count(&self) -> usize {
        self.play_calls.lock().await.len()
    }

    pub fn stop_call_count(&self) -> u64 {
        self.stop_calls.load(Ordering::Relaxed)
    }

    pub async fn set_next_call_count(&self) -> usize {
        self.set_next_calls.lock().await.len()
    }

    pub async fn last_play_url(&self) -> Option<String> {
        self.play_calls.lock().await.last().map(|c| c.url.clone())
    }

    pub async fn last_next_url(&self) -> Option<String> {
        self.set_next_calls
            .lock()
            .await
            .last()
            .map(|c| c.url.clone())
    }

    /// Simulate a gapless transition: renderer moves to the next URI
    /// and reports the new track's duration/position.
    pub async fn simulate_gapless_transition(&self, new_duration_ms: u64) {
        let next = self.next_uri.lock().await.take();
        if let Some(uri) = next {
            *self.current_uri.lock().await = Some(uri);
        }
        self.duration_ms.store(new_duration_ms, Ordering::Relaxed);
        self.position_ms.store(0, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl OutputTarget for MockOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.id
    }

    fn output_type(&self) -> &str {
        "mock"
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        *self.state.lock().await = TransportState::Playing;
        *self.current_uri.lock().await = Some(media.url.to_string());
        self.position_ms.store(0, Ordering::Relaxed);
        self.play_calls.lock().await.push(PlayCall {
            url: media.url.to_string(),
            title: media.title.map(String::from),
        });
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        *self.state.lock().await = TransportState::Paused;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        *self.state.lock().await = TransportState::Playing;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        *self.state.lock().await = TransportState::Stopped;
        self.position_ms.store(0, Ordering::Relaxed);
        self.stop_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        self.position_ms.store(position_ms, Ordering::Relaxed);
        Ok(())
    }

    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }

    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: *self.state.lock().await,
            position_ms: self.position_ms.load(Ordering::Relaxed),
            duration_ms: self.duration_ms.load(Ordering::Relaxed),
            volume: 0.5,
            muted: false,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: None,
            track_artist: None,
        })
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        *self.next_uri.lock().await = Some(media.url.to_string());
        self.set_next_calls.lock().await.push(PlayCall {
            url: media.url.to_string(),
            title: media.title.map(String::from),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_output_play_stop_cycle() {
        let mock = MockOutput::new("test-device", "Test");

        let status = mock.get_status().await.unwrap();
        assert_eq!(status.state, TransportState::Stopped);

        mock.play_media(&PlayMedia {
            url: "http://localhost/stream/123.wav",
            mime_type: "audio/wav",
            title: Some("Track 1"),
            ..Default::default()
        })
        .await
        .unwrap();

        let status = mock.get_status().await.unwrap();
        assert_eq!(status.state, TransportState::Playing);
        assert_eq!(mock.play_call_count().await, 1);

        mock.stop().await.unwrap();
        assert_eq!(mock.stop_call_count(), 1);
        assert_eq!(mock.get_status().await.unwrap().state, TransportState::Stopped);
    }

    #[tokio::test]
    async fn mock_output_gapless_transition() {
        let mock = MockOutput::new("test-device", "Test");

        mock.play_media(&PlayMedia {
            url: "http://localhost/stream/track1.wav",
            mime_type: "audio/wav",
            title: Some("Track 1"),
            ..Default::default()
        })
        .await
        .unwrap();
        mock.set_duration(256_487);
        mock.set_position(246_000);

        mock.set_next_media(&PlayMedia {
            url: "http://localhost/stream/track2.wav",
            mime_type: "audio/wav",
            title: Some("Track 2"),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(mock.set_next_call_count().await, 1);

        mock.simulate_gapless_transition(226_000).await;

        let status = mock.get_status().await.unwrap();
        assert_eq!(status.duration_ms, 226_000);
        assert_eq!(status.position_ms, 0);
        assert_eq!(
            status.current_uri.as_deref(),
            Some("http://localhost/stream/track2.wav")
        );

        // play_media should NOT have been called for the gapless transition
        assert_eq!(mock.play_call_count().await, 1, "gapless should not trigger extra play_media");
        assert_eq!(mock.stop_call_count(), 0, "gapless should not trigger stop");
    }

    #[tokio::test]
    async fn mock_output_tracks_all_calls() {
        let mock = MockOutput::new("d1", "Device 1");

        for i in 0..3 {
            mock.play_media(&PlayMedia {
                url: &format!("http://localhost/stream/{i}.wav"),
                mime_type: "audio/wav",
                ..Default::default()
            })
            .await
            .unwrap();
        }

        assert_eq!(mock.play_call_count().await, 3);
        assert_eq!(
            mock.last_play_url().await.as_deref(),
            Some("http://localhost/stream/2.wav")
        );
    }
}
