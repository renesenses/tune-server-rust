use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, oneshot};
use tracing::info;

use tune_core::db::backend::DbBackend;
use tune_core::db::engine::Engine;
use tune_core::db::sqlite::SqliteDb;
use tune_core::discovery::ssdp::SsdpScanner;
use tune_core::event_bus::EventBus;
use tune_core::health_monitor::{AdvancedHealthMonitor, HealthMonitorConfig};
use tune_core::http::streamer::AudioStreamer;
use tune_core::metadata::suggestions::SuggestionStore;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::ServiceRegistry;
use tune_core::streaming::spotify_connect::SpotifyConnectManager;
use tune_core::upnp_server::UpnpState;

use crate::config::TuneConfig;

#[derive(Clone)]
pub struct AppState {
    pub db: SqliteDb,
    /// Engine-agnostic backend. Points to the same `SqliteDb` by default,
    /// or to a `PostgresBackend` when `TUNE_DATABASE_URL` is a postgres://
    /// DSN and the `postgres` feature is enabled.  Repos ported to
    /// `Arc<dyn DbBackend>` should use this field via `with_backend()`.
    pub backend: Arc<dyn DbBackend>,
    pub streamer: Arc<AudioStreamer>,
    pub playback: Arc<PlaybackManager>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub scanner: Arc<Mutex<SsdpScanner>>,
    pub event_bus: Arc<EventBus>,
    pub upnp: Option<UpnpState>,
    pub config: Arc<TuneConfig>,
    pub http_client: reqwest::Client,
    pub port: u16,
    pub started_at: Instant,
    pub bridge_responses:
        Arc<Mutex<HashMap<String, oneshot::Sender<tune_core::outputs::bridge::BridgeResponse>>>>,
    pub health_monitor: Arc<AdvancedHealthMonitor>,
    pub suggestion_store: Arc<SuggestionStore>,
    pub spotify_connect: Arc<SpotifyConnectManager>,
    pub api_analytics: Arc<tune_core::api_analytics::ApiAnalytics>,
    pub poller_metrics: tune_core::poller::PollerMetricsMap,
    pub rooms: Arc<Mutex<tune_core::collaborative::RoomManager>>,
}

impl AppState {
    pub fn new(db_path: &str, port: u16, tune_config: TuneConfig) -> Result<Self, String> {
        // Engine selection: check TUNE_DATABASE_URL for PostgreSQL, else
        // default to SQLite.
        let selected_engine = tune_config
            .database_url
            .as_deref()
            .map(Engine::from_connection_string)
            .unwrap_or(Engine::Sqlite);

        // Always open SQLite — it serves as the fallback for code paths
        // not yet ported to Arc<dyn DbBackend> and as the local cache
        // even when PG is the primary store.
        let db = SqliteDb::open(db_path)?;
        db.init_schema()?;
        tune_core::db::migrations::run_migrations(&db)?;

        // Build the backend: PG when configured + feature-enabled, else SQLite.
        let backend: Arc<dyn DbBackend> =
            Self::create_backend(selected_engine, &tune_config, &db, db_path)?;

        let streamer = Arc::new(AudioStreamer::new(port));
        let playback = Arc::new(PlaybackManager::new());

        let mut services = ServiceRegistry::new();
        services.register(Box::new(
            tune_core::streaming::tidal::TidalService::with_quality(&tune_config.tidal_quality),
        ));
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
        let event_bus = Arc::new(EventBus::new());

        let mut orch = PlaybackOrchestrator::new(
            backend.clone(),
            playback.clone(),
            streamer.clone(),
            services.clone(),
            outputs.clone(),
            tune_config.advertised_ip.clone(),
        );
        orch.event_bus = Some(event_bus.clone());
        let orchestrator = Arc::new(orch);

        let (ssdp_tx, _) = tokio::sync::mpsc::channel(64);
        let scanner = Arc::new(Mutex::new(SsdpScanner::new(ssdp_tx)));

        let upnp = UpnpState::new(db.clone(), port);

        let health_config = HealthMonitorConfig {
            db_path: db_path.into(),
            ..Default::default()
        };
        let health_monitor = Arc::new(AdvancedHealthMonitor::new(health_config));

        let suggestion_store = Arc::new(SuggestionStore::new(db.clone()));
        suggestion_store.setup_table().ok();

        let spotify_connect = Arc::new(SpotifyConnectManager::new("Tune".into(), port));

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
            .expect("http client init");

        Ok(Self {
            db,
            backend,
            streamer,
            playback,
            services,
            outputs,
            orchestrator,
            scanner,
            event_bus,
            upnp: Some(upnp),
            config: Arc::new(tune_config),
            http_client,
            port,
            started_at: Instant::now(),
            bridge_responses: Arc::new(Mutex::new(HashMap::new())),
            health_monitor,
            suggestion_store,
            spotify_connect,
            api_analytics: Arc::new(tune_core::api_analytics::ApiAnalytics::default()),
            poller_metrics: Arc::new(Mutex::new(std::collections::HashMap::new())),
            rooms: Arc::new(Mutex::new(tune_core::collaborative::RoomManager::new())),
        })
    }

    /// Build the appropriate `DbBackend` based on the selected engine.
    fn create_backend(
        engine: Engine,
        config: &TuneConfig,
        sqlite_db: &SqliteDb,
        db_path: &str,
    ) -> Result<Arc<dyn DbBackend>, String> {
        match engine {
            Engine::Sqlite => {
                info!(engine = "sqlite", path = %db_path, "database_engine_selected");
                Ok(Arc::new(sqlite_db.clone()))
            }
            Engine::Postgres => {
                #[cfg(feature = "postgres")]
                {
                    let pg_url = config
                        .database_url
                        .as_deref()
                        .ok_or("TUNE_DATABASE_URL is required for postgres engine")?;
                    let safe_url = pg_url.split('@').last().unwrap_or(pg_url);
                    info!(engine = "postgres", url = %safe_url, "database_engine_selected");

                    // Connect to PG and run migrations synchronously
                    // (we're inside AppState::new which is sync).
                    let backend = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(async {
                            let pg = tune_core::db::postgres::PostgresDb::connect(pg_url).await?;
                            tune_core::db::migrations::run_pg_migrations(pg.pool()).await?;
                            let backend =
                                tune_core::db::backend::PostgresBackend::new(pg.pool().clone());
                            Ok::<_, String>(Arc::new(backend) as Arc<dyn DbBackend>)
                        })
                    })?;

                    info!("postgres_backend_ready");
                    Ok(backend)
                }

                #[cfg(not(feature = "postgres"))]
                {
                    let _ = config;
                    Err("PostgreSQL engine requested but the `postgres` feature \
                         is not enabled. Rebuild with `--features postgres`."
                        .into())
                }
            }
        }
    }

    pub async fn restore_tokens(&self) {
        let registry = self.services.lock().await;
        registry.restore_all_tokens(&self.backend).await;
    }

    pub async fn save_tokens(&self) {
        let registry = self.services.lock().await;
        registry.save_all_tokens(&self.backend).await;
    }
}
