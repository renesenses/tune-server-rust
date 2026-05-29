use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, oneshot};

use tune_core::db::sqlite::SqliteDb;
use tune_core::discovery::ssdp::SsdpScanner;
use tune_core::event_bus::EventBus;
use tune_core::health_monitor::{AdvancedHealthMonitor, HealthMonitorConfig};
use tune_core::http::streamer::AudioStreamer;
use tune_core::metadata_suggestions::SuggestionStore;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::ServiceRegistry;
use tune_core::upnp_server::UpnpState;

use crate::config::TuneConfig;

#[derive(Clone)]
pub struct AppState {
    pub db: SqliteDb,
    pub streamer: Arc<AudioStreamer>,
    pub playback: Arc<PlaybackManager>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub scanner: Arc<Mutex<SsdpScanner>>,
    pub event_bus: Arc<EventBus>,
    pub upnp: Option<UpnpState>,
    pub config: Arc<TuneConfig>,
    pub port: u16,
    pub started_at: Instant,
    pub bridge_responses:
        Arc<Mutex<HashMap<String, oneshot::Sender<tune_core::outputs::bridge::BridgeResponse>>>>,
    pub health_monitor: Arc<AdvancedHealthMonitor>,
    pub suggestion_store: Arc<SuggestionStore>,
}

impl AppState {
    pub fn new(db_path: &str, port: u16, tune_config: TuneConfig) -> Result<Self, String> {
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
        services.register(Box::new(
            tune_core::streaming::spotify::SpotifyService::with_config(
                tune_config.spotify_client_id.as_deref(),
                tune_config.spotify_redirect_uri.as_deref(),
            ),
        ));
        services.register(Box::new(tune_core::streaming::deezer::DeezerService::new()));
        services.register(Box::new(
            tune_core::streaming::youtube::YouTubeService::new(),
        ));

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

        let event_bus = Arc::new(EventBus::new());

        let upnp = UpnpState::new(db.clone(), port);

        let health_config = HealthMonitorConfig {
            db_path: db_path.into(),
            ..Default::default()
        };
        let health_monitor = Arc::new(AdvancedHealthMonitor::new(health_config));

        let suggestion_store = Arc::new(SuggestionStore::new(db.clone()));
        suggestion_store.setup_table().ok();

        Ok(Self {
            db,
            streamer,
            playback,
            services,
            outputs,
            orchestrator,
            scanner,
            event_bus,
            upnp: Some(upnp),
            config: Arc::new(tune_config),
            port,
            started_at: Instant::now(),
            bridge_responses: Arc::new(Mutex::new(HashMap::new())),
            health_monitor,
            suggestion_store,
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
