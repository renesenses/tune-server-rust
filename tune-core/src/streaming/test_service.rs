//! Network-free witness shared by private transport and plugin SDK tests.
use super::*;
use crate::TuneError;

pub(crate) struct TestService(pub &'static str);

#[async_trait::async_trait]
impl StreamingService for TestService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.0
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _: bool) {}
    fn audio_delivery(&self) -> audio_source::AudioDelivery {
        audio_source::AudioDelivery::DecodedPcm
    }
    async fn open_pcm_audio(
        &self,
        _: &audio_source::PcmRequest<'_>,
    ) -> Result<audio_source::DecodedPcmSource, String> {
        if self.0 == "fixture-decoded" {
            let (tx, source) = audio_source::DecodedPcmSource::channel(
                audio_source::PcmFormat {
                    sample_rate: 44100,
                    bit_depth: 16,
                    channels: 2,
                },
                crate::audio::formats::AudioFormat::Flac,
                serde_json::from_value(serde_json::json!({
                    "id": "pcm", "title": "Generic provider", "artist": "Fixture",
                    "duration_ms": 120, "explicit": false
                }))
                .unwrap(),
            )?;
            tokio::spawn(async move {
                let _ = tx.send(vec![1; 44100 * 4 * 120 / 1000]).await;
            });
            return Ok(source);
        }
        Err("private decoder refused".into())
    }
    async fn authenticate(&mut self, _: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: false,
            username: None,
            subscription: None,
            expires_in: None,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _: &str, _: usize) -> Result<SearchResults, TuneError> {
        Err("not used".into())
    }
    async fn get_track(&self, _: &str) -> Result<StreamTrack, TuneError> {
        Err("not used".into())
    }
    async fn get_track_url(&self, _: &str, _: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("PUBLIC URL PATH MUST NOT BE USED".into())
    }
    async fn get_album(&self, _: &str) -> Result<StreamAlbum, TuneError> {
        Err("not used".into())
    }
    async fn get_album_tracks(&self, _: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn get_artist(&self, _: &str) -> Result<StreamArtist, TuneError> {
        Err("not used".into())
    }
    async fn get_playlist(&self, _: &str) -> Result<StreamPlaylist, TuneError> {
        Err("not used".into())
    }
    async fn get_playlist_tracks(&self, _: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}
