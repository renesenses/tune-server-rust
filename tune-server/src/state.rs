use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use tune_core::db::sqlite::SqliteDb;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::ServiceRegistry;

#[derive(Clone)]
pub struct AppState {
    pub db: SqliteDb,
    pub streamer: Arc<AudioStreamer>,
    pub playback: Arc<PlaybackManager>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
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

        let mut services = ServiceRegistry::new();
        services.register(Box::new(tune_core::streaming::tidal::TidalService::new()));
        services.register(Box::new(tune_core::streaming::qobuz::QobuzService::new(
            std::env::var("QOBUZ_APP_ID").unwrap_or_default(),
            std::env::var("QOBUZ_APP_SECRET").unwrap_or_default(),
        )));
        services.register(Box::new(tune_core::streaming::spotify::SpotifyService::new()));
        services.register(Box::new(tune_core::streaming::deezer::DeezerService::new()));
        services.register(Box::new(tune_core::streaming::youtube::YouTubeService::new()));

        let outputs = OutputRegistry::new();

        Ok(Self {
            db,
            streamer,
            playback,
            services: Arc::new(Mutex::new(services)),
            outputs: Arc::new(Mutex::new(outputs)),
            port,
            started_at: Instant::now(),
        })
    }
}
