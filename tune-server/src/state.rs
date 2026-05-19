use std::sync::Arc;

use tune_core::db::sqlite::SqliteDb;
use tune_core::http::streamer::AudioStreamer;

#[derive(Clone)]
pub struct AppState {
    pub db: SqliteDb,
    pub streamer: Arc<AudioStreamer>,
    pub port: u16,
}

impl AppState {
    pub fn new(db_path: &str, port: u16) -> Result<Self, String> {
        let db = SqliteDb::open(db_path)?;
        db.init_schema()?;
        tune_core::db::migrations::run_migrations(&db)?;

        let streamer = Arc::new(AudioStreamer::new(port));

        Ok(Self { db, streamer, port })
    }
}
