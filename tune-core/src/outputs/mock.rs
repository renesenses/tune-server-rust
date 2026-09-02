use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};

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
    output_type: String,
    host: Option<String>,
    state: Arc<Mutex<TransportState>>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    volume: Arc<Mutex<f64>>,
    muted: Arc<Mutex<bool>>,
    current_uri: Arc<Mutex<Option<String>>>,
    next_uri: Arc<Mutex<Option<String>>>,
    play_calls: Arc<Mutex<Vec<PlayCall>>>,
    stop_calls: Arc<AtomicU64>,
    set_next_calls: Arc<Mutex<Vec<PlayCall>>>,
    /// Chaque `set_volume` REÇUE, dans l'ordre (#2395).
    ///
    /// Le volume courant seul ne peut pas répondre à « l'appareil a-t-il reçu
    /// une commande ? » : une consigne à la valeur déjà en place ne change
    /// rien d'observable, et trois commandes identiques se lisent comme une.
    /// C'est pourtant exactement la question du mode bit-perfect, où le défaut
    /// était de RENVOYER 100 % à chaque piste à un appareil déjà à 100 %.
    volume_calls: Arc<Mutex<Vec<f64>>>,
}

impl MockOutput {
    pub fn new(id: &str, name: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            output_type: "mock".into(),
            host: None,
            state: Arc::new(Mutex::new(TransportState::Stopped)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            volume: Arc::new(Mutex::new(0.5)),
            muted: Arc::new(Mutex::new(false)),
            current_uri: Arc::new(Mutex::new(None)),
            next_uri: Arc::new(Mutex::new(None)),
            play_calls: Arc::new(Mutex::new(Vec::new())),
            stop_calls: Arc::new(AtomicU64::new(0)),
            set_next_calls: Arc::new(Mutex::new(Vec::new())),
            volume_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Override the reported `output_type` (default "mock"), so tests can model
    /// a specific protocol (e.g. "dlna" vs "squeezebox").
    pub fn with_type(mut self, output_type: &str) -> Self {
        self.output_type = output_type.into();
        self
    }

    /// Override the reported `host` (default `None`), so tests can model two
    /// outputs living on the same LMS box (same-device dedup).
    pub fn with_host(mut self, host: &str) -> Self {
        self.host = Some(host.into());
        self
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

    /// Les commandes de volume reçues, dans l'ordre (#2395).
    pub async fn volume_calls(&self) -> Vec<f64> {
        self.volume_calls.lock().await.clone()
    }

    /// Combien de commandes de volume l'appareil a REÇUES (#2395).
    ///
    /// `0` est une réponse utile, et c'est même la plus fréquente ici : elle
    /// prouve qu'aucune commande n'est partie, ce qu'un volume courant à 0,5
    /// ne prouverait pas.
    pub async fn volume_call_count(&self) -> usize {
        self.volume_calls.lock().await.len()
    }

    pub async fn last_play_url(&self) -> Option<String> {
        self.play_calls.lock().await.last().map(|c| c.url.clone())
    }

    /// Les titres passes a `set_next_media`, DANS L'ORDRE (#3026).
    ///
    /// Le COMPTE d'appels ne dit pas *quelle* piste a ete armee, et l'URL d'un
    /// flux local est un identifiant de session opaque. Le titre est ce que le
    /// renderer recoit dans ses metadonnees, donc ce que l'ecran doit nommer :
    /// c'est la seule grandeur qui permette de comparer « ce qui part au
    /// renderer » a « ce que la file affiche ».
    pub async fn set_next_titles(&self) -> Vec<String> {
        self.set_next_calls
            .lock()
            .await
            .iter()
            .map(|c| c.title.clone().unwrap_or_default())
            .collect()
    }

    /// Les titres passes a `play_media`, DANS L'ORDRE.
    pub async fn play_titles(&self) -> Vec<String> {
        self.play_calls
            .lock()
            .await
            .iter()
            .map(|c| c.title.clone().unwrap_or_default())
            .collect()
    }

    /// L'URI que le renderer joue reellement.
    pub async fn current_uri(&self) -> Option<String> {
        self.current_uri.lock().await.clone()
    }

    /// Le TITRE de ce que le renderer joue reellement, retrouve par l'URI telle
    /// qu'elle lui a ete donnee. C'est la correspondance qui manque au journal
    /// de #3026 : l'ecran doit nommer le flux physiquement en cours, pas un
    /// index de file.
    pub async fn current_title(&self) -> Option<String> {
        let uri = self.current_uri.lock().await.clone()?;
        let joue = self
            .play_calls
            .lock()
            .await
            .iter()
            .find(|c| c.url == uri)
            .and_then(|c| c.title.clone());
        if joue.is_some() {
            return joue;
        }
        self.set_next_calls
            .lock()
            .await
            .iter()
            .find(|c| c.url == uri)
            .and_then(|c| c.title.clone())
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
        &self.output_type
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }

    fn host(&self) -> Option<&str> {
        self.host.as_deref()
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

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // La consigne est enregistrée TELLE QUE REÇUE, avant le clamp : le
        // journal des commandes doit dire ce que l'appareil a reçu, pas ce
        // qu'un mock bienveillant en a fait.
        self.volume_calls.lock().await.push(volume);
        *self.volume.lock().await = volume.clamp(0.0, 1.0);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        *self.muted.lock().await = muted;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: *self.state.lock().await,
            position_ms: self.position_ms.load(Ordering::Relaxed),
            duration_ms: self.duration_ms.load(Ordering::Relaxed),
            volume: *self.volume.lock().await,
            muted: *self.muted.lock().await,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: None,
            track_artist: None,
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
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
        assert_eq!(
            mock.get_status().await.unwrap().state,
            TransportState::Stopped
        );
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
        assert_eq!(
            mock.play_call_count().await,
            1,
            "gapless should not trigger extra play_media"
        );
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
