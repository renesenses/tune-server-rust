use std::sync::Arc;
use std::time::Instant;

use tune_core::db::sqlite::SqliteDb;
use tune_core::http::streamer::AudioStreamer;
use tune_core::playback::PlaybackManager;

#[derive(Clone)]
pub struct AppState {
    pub db: SqliteDb,
    pub streamer: Arc<AudioStreamer>,
    pub playback: Arc<PlaybackManager>,
    pub port: u16,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(db_path: &str, port: u16) -> Result<Self, String> {
        let db = SqliteDb::open(db_path)?;
        db.init_schema()?;
        tune_core::db::migrations::run_migrations(&db)?;

        let streamer = Arc::new(AudioStreamer::new(port));
        let playback = Arc::new(PlaybackManager::new());

        Ok(Self {
            db,
            streamer,
            playback,
            port,
            started_at: Instant::now(),
        })
    }
}
