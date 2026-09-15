//! Subprocess entry, dispatched BEFORE configuration, databases or logging.
use super::{
    engine::SpotifyNativeService as Engine,
    ipc::{self, Failure, Operation, Reply},
};
use crate::{TuneError, streaming::traits::*};
use serde::Serialize;
use serde_json::{Value, json};

/// Called by every server/composer bootstrap. Never starts the Tune service in
/// a worker; all secrets are read from inherited anonymous pipes, not argv.
pub async fn run_worker_if_requested() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("--spotify-native-worker") {
        return;
    }
    initialize_tls();
    let result = match args.as_slice() {
        [_, mode] if mode == "control" => control().await,
        [_, mode] if mode == "audio" => super::audio::run_audio_worker().await,
        _ => Err("Invalid Spotify worker invocation".into()),
    };
    // Only the child reaches this branch. In particular, librespot exit(1)
    // and a broken PCM pipe have no path to the parent's process::exit.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

pub(super) fn initialize_tls() {
    // The child bypasses the server bootstrap, including its TLS setup.
    // tune-core directly enables rustls's default aws-lc provider; with ring
    // also present transitively, rustls cannot choose one automatically.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn encode<T: Serialize>(result: Result<T, TuneError>) -> Result<Value, TuneError> {
    serde_json::to_value(result?).map_err(|_| "Spotify worker serialization failed".into())
}

pub(super) async fn execute(engine: &mut Engine, operation: Operation) -> Result<Value, TuneError> {
    match operation {
        Operation::Init { tokens } => {
            if engine.restore_tokens(&tokens) {
                engine.post_restore().await;
            }
            Ok(Value::Null)
        }
        Operation::Status => encode(Ok(engine.poll_status().await)),
        Operation::Pair => encode(engine.authenticate(&json!({})).await),
        Operation::Search { query, limit } => encode(engine.search(&query, limit).await),
        Operation::Track { id } => encode(engine.get_track(&id).await),
        Operation::Album { id } => encode(engine.get_album(&id).await),
        Operation::AlbumTracks { id } => encode(engine.get_album_tracks(&id).await),
        Operation::Artist { id } => encode(engine.get_artist(&id).await),
        Operation::ArtistAlbums { id } => encode(engine.get_artist_albums(&id).await),
        Operation::Playlist { id } => encode(engine.get_playlist(&id).await),
        Operation::PlaylistTracks { id } => encode(engine.get_playlist_tracks(&id).await),
        Operation::UserPlaylists => encode(engine.get_user_playlists().await),
        Operation::UserTracks => encode(engine.get_user_tracks().await),
        Operation::UserAlbums => encode(engine.get_user_albums().await),
        Operation::UserArtists => encode(engine.get_user_artists().await),
        Operation::PlaylistLibrary => encode(engine.get_playlist_library().await),
        Operation::Play { .. } => Err("Audio operations require an audio worker".into()),
    }
}

async fn control() -> Result<(), String> {
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let mut engine = Engine::new();
    let mut initialized = false;
    loop {
        let operation: Operation = ipc::read_frame(&mut input).await?;
        // Exactly one Init per child. No accidental restoration in the middle
        // of pairing or an active authenticated session.
        if matches!(operation, Operation::Init { .. }) == initialized {
            return Err("Spotify worker initialization protocol violated".into());
        }
        initialized = true;
        let result = execute(&mut engine, operation)
            .await
            .map_err(Failure::from_tune);
        let reply = Reply {
            result,
            status: engine.auth_status().await,
            details: engine.pairing_status(),
            tokens: engine.save_tokens().unwrap_or(Value::Null),
        };
        ipc::write_frame(&mut output, &reply).await?;
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn native_worker_initializes_tls_before_session_creation() {
        super::initialize_tls();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "Spotify worker needs its own TLS provider before creating a session"
        );
        let session = librespot_core::Session::new(Default::default(), None);
        session.shutdown();
    }
}
