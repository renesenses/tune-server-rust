use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use tune_core::db::sqlite::SqliteDb;
use tune_core::discovery::ssdp::SsdpScanner;
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
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub scanner: Arc<Mutex<SsdpScanner>>,
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

        let services = Arc::new(Mutex::new(services));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));

        let orchestrator = Arc::new(PlaybackOrchestrator {
            db: db.clone(),
            playback: playback.clone(),
            streamer: streamer.clone(),
            services: services.clone(),
            outputs: outputs.clone(),
        });

        let (ssdp_tx, _) = tokio::sync::mpsc::channel(64);
        let scanner = Arc::new(Mutex::new(SsdpScanner::new(ssdp_tx)));

        Ok(Self {
            db,
            streamer,
            playback,
            services,
            outputs,
            orchestrator,
            scanner,
            port,
            started_at: Instant::now(),
        })
    }

    pub async fn restore_tokens(&self) {
        let registry = self.services.lock().await;
        registry.restore_all_tokens(&self.db).await;
    }

    pub async fn save_tokens(&self) {
        let registry = self.services.lock().await;
        registry.save_all_tokens(&self.db).await;
    }
}
