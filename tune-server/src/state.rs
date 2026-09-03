use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
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
    /// The raw SQLite handle — `None` in PostgreSQL mode, where no SQLite
    /// database is opened at all.
    ///
    /// Reach for [`Self::backend`] instead. This exists only for the handful of
    /// operations with no engine-agnostic equivalent: SQLite-specific
    /// maintenance (`VACUUM`, `wal_checkpoint`), the schema-version readout,
    /// and the SQLite→PG migration, which needs both stores at once by
    /// definition. Anything that serves user data must go through `backend`, or
    /// it will read a different library from the rest of the server.
    pub db: Option<SqliteDb>,
    /// Engine-agnostic backend — the single source of truth. Points to the
    /// `SqliteDb` above in SQLite mode, or to a `PostgresBackend` when
    /// `TUNE_DATABASE_URL` is a postgres:// DSN and the `postgres` feature is
    /// enabled.
    pub backend: Arc<dyn DbBackend>,
    pub streamer: Arc<AudioStreamer>,
    pub playback: Arc<PlaybackManager>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    /// Scanner SSDP partagé SANS mutex englobant : `rescan()` ne prend que
    /// `&self` et verrouille en interne. Un mutex ici sérialisait tous les
    /// lecteurs pendant chaque balayage réseau (#1432).
    pub scanner: Arc<SsdpScanner>,
    pub event_bus: Arc<EventBus>,
    /// Registry of in-progress background tasks (enrichment, artwork, bios) for
    /// the UI "tâches de fond" indicator. See [`crate::background_tasks`].
    pub background_tasks: crate::background_tasks::BackgroundTasks,
    pub upnp: Option<UpnpState>,
    pub config: Arc<TuneConfig>,
    pub http_client: reqwest::Client,
    pub port: u16,
    /// Origine du compteur `uptime_seconds` : un `Instant` capturé à la
    /// construction de l'état, donc AU DÉMARRAGE DU PROCESSUS. Il repart
    /// nécessairement de zéro à chaque redémarrage — un `Instant` n'est ni
    /// sérialisable ni persistable, et rien ne le restaure.
    pub started_at: Instant,
    /// Le même instant, mais en horloge absolue (UTC) — #2117.
    ///
    /// `started_at` seul ne permet pas de répondre à « le serveur a-t-il
    /// redémarré ? » : c'est un compteur RELATIF, et deux appels espacés de
    /// deux minutes qui rendent 4765 puis 4903 se lisent aussi bien comme
    /// « pas de redémarrage » que comme « je regarde un compteur qui ne dit
    /// pas ce que je crois ». Il faut inférer, et l'inférence s'est révélée
    /// fausse en diagnostic réel.
    ///
    /// Un horodatage absolu ne demande aucune inférence : il CHANGE au
    /// redémarrage et reste identique tant que le processus vit. Deux appels
    /// suffisent à trancher, sans arithmétique.
    pub process_started_at: time::OffsetDateTime,
    pub bridge_responses:
        Arc<Mutex<HashMap<String, oneshot::Sender<tune_core::outputs::bridge::BridgeResponse>>>>,
    pub health_monitor: Arc<AdvancedHealthMonitor>,
    pub suggestion_store: Arc<SuggestionStore>,
    pub spotify_connect: Arc<SpotifyConnectManager>,
    pub api_analytics: Arc<tune_core::api_analytics::ApiAnalytics>,
    pub poller_metrics: tune_core::poller::PollerMetricsMap,
    pub update_phase: Arc<std::sync::Mutex<Option<String>>>,
    pub rooms: Arc<Mutex<tune_core::collaborative::RoomManager>>,
    pub media_servers: Arc<Mutex<HashMap<String, tune_core::discovery::ssdp::MediaServerInfo>>>,
    /// mDNS scanner handle, populated by
    /// [`crate::discovery_setup::spawn_mdns_handler`] once discovery starts. Kept
    /// here (not just as a local `_mdns_handle`) so routes can list the peer Tune
    /// servers it browses on `_tune-server._tcp` (#1273). `None` until the mDNS
    /// daemon starts, or if it failed to start.
    pub mdns_scanner: Arc<std::sync::Mutex<Option<Arc<tune_core::discovery::mdns::MdnsScanner>>>>,
    /// The backend the registered local outputs were actually built with,
    /// published by [`crate::startup::register_local_outputs`]. `None` until
    /// they are registered (or when the build has no local audio).
    ///
    /// Distinct from the *stored* preference on purpose: picking ASIO in the
    /// settings page only takes effect on the next start, so between the two
    /// the honest answer is still WASAPI.
    pub active_audio_backend: Arc<std::sync::RwLock<Option<String>>>,
    pub license: Arc<tune_core::license::LicenseManager>,
    pub skin_manager: Arc<tune_core::skins::SkinManager>,
    /// Compiled-in plugins. Empty until [`crate::plugins::init`] runs, which
    /// happens after local outputs are registered and before the router is
    /// built. See [`crate::plugins`].
    pub plugins: Arc<Mutex<tune_core::plugin_sdk::PluginLoader>>,
    /// What [`crate::plugins::init`] actually loaded, published once and never
    /// mutated again — no plugin registers after init.
    ///
    /// The `/api/v1/plugins` handlers read this instead of the loader on
    /// purpose: event dispatch holds the loader's lock across *every* plugin's
    /// `on_event`, so an introspection request that took the same lock would
    /// hang for as long as the slowest plugin, and would hold `plugins` while
    /// doing so — delaying shutdown too.
    pub plugin_info: Arc<OnceLock<Vec<tune_core::plugin_sdk::PluginInfo>>>,
    /// Compiled-in plugins that did not load at startup — opt-in ones the user
    /// has not installed, or ones they disabled. Published alongside
    /// [`Self::plugin_info`] so the plugin manager can list a dormant plugin
    /// (offering "Install"/"Enable") instead of it vanishing from the API.
    pub plugin_available: Arc<OnceLock<Vec<tune_core::plugin_sdk::AvailablePluginInfo>>>,
    /// Every compiled-in plugin name, published by [`crate::plugins::init`]
    /// before `setup_all` runs — so it holds the whole registered set, not the
    /// part that loaded.
    ///
    /// Wider than [`Self::plugin_info`] ∪ [`Self::plugin_available`] on
    /// purpose: an uncatalogued plugin (DJ, Karaoke — #2090) appears in
    /// neither, yet installing it by name is still meant to work. This is what
    /// `POST /plugins/{name}/install` checks a name against, so it can refuse
    /// one that names nothing this binary carries (#2132).
    pub plugin_names: Arc<OnceLock<Vec<String>>>,
    /// Loaded WASM plugins (P2 of the plugin ABI). Published once by
    /// [`crate::plugins_host::load_wasm_plugins`] at startup and read by the
    /// `/api/v1/plugins/{id}/…` route mount. Gated behind `plugins-wasm`, so
    /// the default server carries neither this field nor wasmtime.
    #[cfg(feature = "plugins-wasm")]
    pub wasm_plugins: Arc<OnceLock<crate::plugins_host::WasmRegistry>>,
    #[cfg(feature = "cloud-relay")]
    pub relay_client: Option<Arc<tune_core::cloud::relay::RelayClient>>,
}

impl axum::extract::FromRef<AppState> for tune_streaming_http::StreamingHttpState {
    fn from_ref(state: &AppState) -> Self {
        Self::new(
            state.backend.clone(),
            state.services.clone(),
            state.event_bus.clone(),
        )
    }
}

impl axum::extract::FromRef<AppState> for tune_smart_http::SmartHttpState {
    fn from_ref(state: &AppState) -> Self {
        Self::new(state.backend.clone())
    }
}

impl AppState {
    /// L'horodatage absolu du démarrage du processus, en RFC 3339 (UTC).
    ///
    /// Publié à côté de `uptime_seconds` par les réponses de diagnostic
    /// (#2117). Une seule implémentation : trois routes l'exposent, elles
    /// doivent rendre exactement la même chaîne pour le même processus, sans
    /// quoi la comparaison entre deux appels ne prouve plus rien.
    pub fn process_started_at_rfc3339(&self) -> String {
        self.process_started_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    /// The SQLite handle, for the few operations with no engine-agnostic
    /// equivalent (FTS rebuild, `VACUUM`, WAL checkpoint, schema version).
    ///
    /// Errors in PostgreSQL mode instead of silently doing nothing: these are
    /// operator-facing endpoints, and "it returned 200 and changed nothing" is
    /// the failure mode this whole change exists to remove.
    pub fn sqlite(&self) -> Result<&SqliteDb, String> {
        self.db.as_ref().ok_or_else(|| {
            "this operation is SQLite-specific and the server is running on PostgreSQL".to_string()
        })
    }

    /// The audio backend the local outputs are *displayed* as using: what they
    /// were actually built with, falling back to the stored preference before
    /// they exist.
    ///
    /// Reading `config.local_audio_backend` here — as every display path used
    /// to — means someone who picks ASIO in the settings page is told "WASAPI"
    /// forever, in the signal path, in the diagnostics and in the device list,
    /// because the choice is stored in the database, never in the config file
    /// (forum, Windows).
    pub fn display_audio_backend(&self) -> String {
        if let Ok(guard) = self.active_audio_backend.read() {
            if let Some(b) = guard.as_ref() {
                return b.clone();
            }
        }
        self.effective_audio_backend()
    }

    /// Whether local outputs should open the device in exclusive (bit-perfect)
    /// mode: the stored setting wins over the config file.
    ///
    /// Same trap as [`Self::effective_audio_backend`], with teeth: the settings
    /// page writes this to the database, and the outputs were built from the
    /// config file — so the "partagé / exclusif" selector did nothing at all,
    /// restart or not. Selecting ASIO in that page also stores
    /// `local_exclusive_mode: true`, which is what arms the ASIO exclusive
    /// path, so it was silently lost too.
    pub fn effective_exclusive_mode(&self) -> bool {
        self.exclusive_mode_status().effective
    }

    /// Le réglage tel que l'utilisateur l'a DEMANDÉ, avant toute contrainte de
    /// plateforme : la base d'abord, le fichier de config en repli.
    ///
    /// Séparé de [`Self::effective_exclusive_mode`] parce que la page de
    /// réglages doit pouvoir montrer les deux — c'est tout l'objet de #3192 :
    /// tant qu'un seul nombre existait, l'écran ne pouvait pas dire que le
    /// choix affiché n'était pas celui qui s'appliquait.
    pub fn requested_exclusive_mode(&self) -> bool {
        tune_core::db::settings_repo::SettingsRepo::with_backend(self.backend.clone())
            .get("local_exclusive_mode")
            .ok()
            .flatten()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(self.config.local_exclusive_mode)
    }

    /// Ce que le mode exclusif vaut réellement, ce qui a été demandé, et
    /// pourquoi les deux diffèrent quand c'est le cas.
    ///
    /// ASIO is exclusive by nature — the shared path can't drive it. Mirrors
    /// the same rule applied to the config file at load time.
    ///
    /// #1268 — mais c'est une notion WINDOWS, et cette règle ne le disait
    /// pas. `local_audio_backend = "asio"` se trouve en base sur des
    /// serveurs macOS et Linux : une bibliothèque migrée depuis une machine
    /// Windows l'y apporte telle quelle, et le sélecteur du client web
    /// propose encore ASIO partout (c'est le symptôme du ticket). Aucun de
    /// ces serveurs ne peut ouvrir un host ASIO — `select_host` sort par le
    /// host par défaut — mais cette ligne armait quand même le mode
    /// exclusif, c'est-à-dire, sur macOS, le hog mode CoreAudio que
    /// personne n'avait demandé. Sous Windows, rien ne change.
    ///
    /// #3192 — jfpaquet, Asus Essence STX II : sous Windows, ce même « rien ne
    /// change » avait un coût que personne ne lui annonçait. Décocher « mode
    /// exclusif » restait sans effet dès que le backend était ASIO, Tune
    /// prenait le périphérique en entier, et le son de toutes les autres
    /// applications disparaissait sans un mot. La RÈGLE est juste — un pilote
    /// ASIO ouvert en partagé n'existe pas — et elle ne bouge pas d'un iota
    /// ici. Ce qui change, c'est qu'elle se DÉCLARE : la contrainte sort par
    /// `/system/config` pour que la case soit verrouillée et expliquée au
    /// lieu d'être ignorée en silence.
    pub fn exclusive_mode_status(&self) -> tune_core::config::ExclusiveModeStatus {
        tune_core::config::local_exclusive_mode_status(
            &self.effective_audio_backend(),
            self.requested_exclusive_mode(),
        )
    }

    /// The audio backend local outputs *should* be built with: the stored
    /// setting (written by the settings page) wins over the config file.
    pub fn effective_audio_backend(&self) -> String {
        tune_core::db::settings_repo::SettingsRepo::with_backend(self.backend.clone())
            .get("local_audio_backend")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.config.local_audio_backend.clone())
    }

    pub fn new(db_path: &str, port: u16, tune_config: TuneConfig) -> Result<Self, String> {
        // Engine selection: check TUNE_DATABASE_URL for PostgreSQL, else
        // default to SQLite.
        let selected_engine = tune_config
            .database_url
            .as_deref()
            .map(Engine::from_connection_string)
            .unwrap_or(Engine::Sqlite);

        // Open and migrate SQLite only when SQLite is the selected engine.
        //
        // This used to run unconditionally, "as the fallback for code paths not
        // yet ported". In PostgreSQL mode that produced two live databases: the
        // one the repos wrote to, and a SQLite file that UPnP, the suggestion
        // store and the backup routes went on reading and writing. Browsing
        // over UPnP showed a different library from the web client, and a
        // backup silently archived the wrong database. Every one of those
        // consumers now goes through `backend`, so there is nothing left to
        // fall back to and no reason to keep a second store open.
        let sqlite_db = match selected_engine {
            Engine::Sqlite => {
                let db = SqliteDb::open(db_path)?;
                db.init_schema()?;
                tune_core::db::migrations::run_migrations(&db)?;
                Some(db)
            }
            Engine::Postgres => None,
        };

        // Build the backend: PG when configured + feature-enabled, else SQLite.
        let backend: Arc<dyn DbBackend> =
            Self::create_backend(selected_engine, &tune_config, sqlite_db.as_ref(), db_path)?;

        // Clean up any leftover temp transcode files from a previous crash.
        tune_core::http::streamer::cleanup_leftover_transcode_files();

        let license = Arc::new(tune_core::license::LicenseManager::new_with_limit(
            backend.clone(),
            tune_config.free_max_zones,
        ));

        let streamer = Arc::new(AudioStreamer::new(port));
        let playback = Arc::new(PlaybackManager::new());

        let mut services = ServiceRegistry::new();
        services.register(Box::new(
            tune_core::streaming::tidal::TidalService::with_quality(&tune_config.tidal_quality),
        ));
        // Qobuz endpoint order: direct-first for everyone; proxy-first only for
        // founder accounts (persisted `qobuz_proxy_first` flag from the cloud
        // license validation — see LicenseManager::set_qobuz_proxy_first).
        let qobuz_proxy_first = {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
            settings
                .get("qobuz_proxy_first")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false)
        };
        let mut qobuz = tune_core::streaming::qobuz::QobuzService::new(
            std::env::var("QOBUZ_APP_ID").unwrap_or_default(),
            std::env::var("QOBUZ_APP_SECRET").unwrap_or_default(),
        );
        qobuz.set_proxy_first(qobuz_proxy_first);
        services.register(Box::new(qobuz));
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
        // 🔴 #2702 / #2778 — Bandcamp n'était NULLE PART dans ce registre.
        // Les deux seules routes qui construisent une file complète
        // (`POST /zones/{id}/play` avec `streaming_album_id` ou
        // `streaming_playlist_id`) commencent par `registry.get(source)` et
        // répondaient donc `400 unknown service: bandcamp` : il ne restait que
        // le chemin « piste distante seule », qui pose une file d'exactement
        // UNE piste — d'où « les morceaux ne s'enchaînent pas » (Sevy Tabroc).
        //
        // L'inscription est INCONDITIONNELLE, comme les cinq autres, et non
        // gardée par l'état du greffon : `ServiceRegistry::get` ne consulte
        // jamais `enabled`, et le service rend `enabled() == false` tant que
        // personne ne l'a activé — il apparaît donc comme disponible et non
        // connecté, exactement comme le greffon opt-in dont il est la seconde
        // face. La LECTURE, elle, ne change pas de chemin : `resolve_stream`
        // route toujours `source == "bandcamp"` vers `resolve_direct_url`.
        #[cfg(feature = "bandcamp")]
        services.register(Box::new(tune_bandcamp::BandcampService::new(
            backend.clone(),
        )));

        let services = Arc::new(Mutex::new(services));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        let event_bus = Arc::new(EventBus::new());
        let background_tasks = crate::background_tasks::BackgroundTasks::new(event_bus.clone());

        let mut orch = PlaybackOrchestrator::new(
            backend.clone(),
            playback.clone(),
            streamer.clone(),
            services.clone(),
            outputs.clone(),
            tune_config.advertised_ip.clone(),
        );
        orch.event_bus = Some(event_bus.clone());
        orch.license = Some(license.clone());
        let orchestrator = Arc::new(orch);

        let (ssdp_tx, _) = tokio::sync::mpsc::channel(64);
        let scanner = Arc::new(SsdpScanner::new(ssdp_tx));

        let upnp = UpnpState::new(backend.clone(), port, tune_config.advertised_ip.clone());

        let health_config = HealthMonitorConfig {
            db_path: db_path.into(),
            ..Default::default()
        };
        let health_monitor = Arc::new(AdvancedHealthMonitor::new(health_config));

        let suggestion_store = Arc::new(SuggestionStore::with_backend(backend.clone()));
        suggestion_store.setup_table().ok();

        let spotify_connect = Arc::new(SpotifyConnectManager::new("Tune".into(), port));

        let http_client = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
            .expect("http client init");

        let web_dir = crate::config::resolve_web_dir();
        let skins_dir = std::env::var("TUNE_SKINS_DIR").unwrap_or_else(|_| "skins".into());
        let skin_manager = Arc::new(tune_core::skins::SkinManager::new(
            std::path::PathBuf::from(skins_dir),
            web_dir,
        ));
        skin_manager.ensure_dirs();

        let plugins = Arc::new(Mutex::new(crate::plugins::build_loader(
            &event_bus,
            backend.clone(),
        )));

        Ok(Self {
            db: sqlite_db,
            backend,
            streamer,
            playback,
            services,
            outputs,
            orchestrator,
            scanner,
            event_bus,
            background_tasks,
            upnp: Some(upnp),
            config: Arc::new(tune_config),
            http_client,
            port,
            started_at: Instant::now(),
            process_started_at: time::OffsetDateTime::now_utc(),
            bridge_responses: Arc::new(Mutex::new(HashMap::new())),
            health_monitor,
            suggestion_store,
            spotify_connect,
            api_analytics: Arc::new(tune_core::api_analytics::ApiAnalytics::default()),
            poller_metrics: Arc::new(Mutex::new(std::collections::HashMap::new())),
            update_phase: Arc::new(std::sync::Mutex::new(None)),
            rooms: Arc::new(Mutex::new(tune_core::collaborative::RoomManager::new())),
            media_servers: Arc::new(Mutex::new(HashMap::new())),
            mdns_scanner: Arc::new(std::sync::Mutex::new(None)),
            active_audio_backend: Arc::new(std::sync::RwLock::new(None)),
            license,
            skin_manager,
            plugins,
            plugin_info: Arc::new(OnceLock::new()),
            plugin_available: Arc::new(OnceLock::new()),
            plugin_names: Arc::new(OnceLock::new()),
            #[cfg(feature = "plugins-wasm")]
            wasm_plugins: Arc::new(OnceLock::new()),
            #[cfg(feature = "cloud-relay")]
            relay_client: None,
        })
    }

    /// Peer Tune servers discovered on the LAN via mDNS (`_tune-server._tcp`).
    ///
    /// Reads the live mDNS scanner (populated by
    /// [`crate::discovery_setup::spawn_mdns_handler`], which browses peers tagged
    /// [`OutputType::Local`]) and drops our own advertisement. Returns an empty
    /// list before discovery starts or when multicast is blocked (Docker macvlan,
    /// Windows firewall) — the manually-added peer list (`/system/peers`) is the
    /// robust fallback for those networks. Shared by `/peers` and
    /// `/system/discover-servers`.
    pub async fn discovered_tune_peers(&self) -> Vec<serde_json::Value> {
        use tune_core::discovery::device::OutputType;
        let scanner = { self.mdns_scanner.lock().unwrap().clone() };
        let Some(scanner) = scanner else {
            return Vec::new();
        };
        let self_ip = tune_core::discovery::ssdp::get_local_ip().map(|ip| ip.to_string());
        scanner
            .devices()
            .await
            .into_iter()
            .filter(|d| d.device_type == OutputType::Local)
            .filter(|d| self_ip.as_deref() != Some(d.host.as_str()))
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "name": d.name,
                    "host": d.host,
                    "port": d.port,
                    "available": d.available,
                    "version": d
                        .capabilities
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                })
            })
            .collect()
    }

    /// Build the appropriate `DbBackend` based on the selected engine.
    fn create_backend(
        engine: Engine,
        config: &TuneConfig,
        sqlite_db: Option<&SqliteDb>,
        db_path: &str,
    ) -> Result<Arc<dyn DbBackend>, String> {
        match engine {
            Engine::Sqlite => {
                info!(engine = "sqlite", path = %db_path, "database_engine_selected");
                let db = sqlite_db
                    .ok_or("internal error: SQLite engine selected but no database was opened")?;
                Ok(Arc::new(db.clone()))
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
