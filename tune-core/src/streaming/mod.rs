pub mod amazon;
pub mod deezer;
pub mod deezer_decrypt;
pub mod favorites_date;
pub mod favorites_import;
pub mod matching;
pub mod podcasts;
pub mod qobuz;
pub mod quality;
pub mod radiofrance;
pub mod registry;
pub mod spotify;
pub mod spotify_connect;
#[cfg(feature = "spotify-native")]
pub mod spotify_native;
pub mod tidal;
pub mod traits;
pub mod youtube;

pub use registry::ServiceRegistry;
pub use traits::*;

/// Native Spotify is both build-time and runtime opt-in. Existing installations
/// keep their Web API service, tokens and authentication flow unchanged.
pub fn configured_spotify(
    client_id: Option<&str>,
    redirect_uri: Option<&str>,
    port: u16,
) -> Box<dyn StreamingService> {
    #[cfg(feature = "spotify-native")]
    if std::env::var("TUNE_SPOTIFY_NATIVE").as_deref() == Ok("1") {
        return Box::new(spotify_native::SpotifyNativeService::new());
    }
    Box::new(spotify::SpotifyService::with_config(
        client_id,
        redirect_uri,
        port,
    ))
}
