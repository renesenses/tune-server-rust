//! Server startup, as a library function.
//!
//! `main.rs` is a shim over [`run`]. The point is composition: a binary that
//! lives outside this repository can call [`run`] with its own plugins, so a
//! closed-source plugin never needs a `path` dependency here — which would
//! break `cargo check` for every clone, since cargo resolves optional path
//! dependencies while writing the lockfile.
//!
//! ```no_run
//! # use tune_server::state::AppState;
//! # use tune_core::plugin_sdk::TunePlugin;
//! # fn my_plugin(_: &AppState) -> Box<dyn TunePlugin> { unimplemented!() }
//! #[tokio::main]
//! async fn main() {
//!     tune_server::run(Some(Box::new(|state: &AppState| {
//!         vec![my_plugin(state)]
//!     })))
//!     .await;
//! }
//! ```

use std::net::SocketAddr;

use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::{self, TuneConfig};
use crate::plugins::PluginBuilder;
use crate::routes;
use crate::state::AppState;

/// Ce dont un binaire composeur a besoin pour demarrer le serveur.
///
/// Cette couture existe pour les caisses de sortie hors-arbre, qui ne peuvent
/// pas apparaitre dans le graphe de dependances public — le premier
/// consommateur est `tune-diretta`, prive.
///
/// ⚠️ Elle a deja ete supprimee une fois (#1510, entre 0.9.69 et 0.9.70) parce
/// que rien, DANS CE DEPOT, ne l'appelait ni ne la testait. Le raisonnement
/// etait juste et la conclusion fausse : un consommateur externe etait casse
/// pendant deux versions. Le test `run_options_carry_output_providers` existe
/// pour que le prochain audit trouve un appelant.
///
/// `..Default::default()` est deliberement supportable : c'est la forme
/// d'appel qu'utilisent les binaires composeurs.
#[derive(Default)]
pub struct RunOptions {
    /// Appele une fois, apres construction de [`AppState`] et enregistrement
    /// des sorties locales, pour produire des greffons a enregistrer aux cotes
    /// de ceux compiles dans le binaire.
    pub build_plugins: Option<PluginBuilder>,
    /// Fournisseurs de sorties hors-arbre. Interroges au demarrage puis
    /// toutes les 60 s — c'est ce polling, et non un enregistrement statique,
    /// que reclame une decouverte reseau dynamique doublee d'une
    /// reverification periodique d'habilitation.
    pub output_providers: Vec<std::sync::Arc<dyn tune_core::outputs::traits::OutputProvider>>,
}

/// Start the server and serve until a shutdown signal arrives.
///
/// `build_plugins` is called once, after [`AppState`] is built and local
/// outputs are registered, to produce plugins to register alongside the
/// compiled-in ones. Pass `None` for the plain server.
///
/// Conserve pour les appelants existants : delegue a [`run_with`].
pub async fn run(build_plugins: Option<PluginBuilder>) {
    run_with(RunOptions {
        build_plugins,
        ..Default::default()
    })
    .await
}

/// Comme [`run`], mais pour un binaire composeur qui apporte ses propres
/// fournisseurs de sorties.
pub async fn run_with(opts: RunOptions) {
    let build_plugins = opts.build_plugins;
    // Probe-child dispatch FIRST: when spawned as a wasm-load probe, do the
    // one dangerous thing and exit before any server state exists (#1249).
    crate::plugins::maybe_run_wasm_probe();

    // `--version` / `-V` : répondre et sortir avant tout effet de bord.
    //
    // Ici et non dans `main.rs` : ce dernier est délibérément vide pour qu'un
    // binaire composeur (`tune-server-diretta`) partage ce démarrage, et il
    // doit hériter du drapeau. Et après la sonde wasm ci-dessus, dont le
    // commentaire impose qu'elle reste le premier geste.
    //
    // Pourquoi ce drapeau existe : l'écran d'une appliance Tune OS affichait
    // la version gravée à l'installation, définitivement. Philippe Landes a lu
    // 0.9.85 dans l'interface web et 0.9.83 en console — sur la MÊME machine,
    // après une mise à jour qui avait parfaitement fonctionné. Faute de pouvoir
    // demander sa version au binaire, l'écran ne pouvait que répéter une valeur
    // figée. Cf tune-os#27.
    if version_requested(std::env::args().skip(1)) {
        println!("tune-server {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    // On Windows, catch panics early and log to file so users can report crashes
    // instead of seeing "tune-server.exe has stopped working" with no info.
    #[cfg(windows)]
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let bt = std::backtrace::Backtrace::force_capture();
            let msg = format!("PANIC: {info}\n\nBacktrace:\n{bt}");
            eprintln!("{msg}");
            let log_dir = std::env::var("LOCALAPPDATA")
                .map(|d| std::path::PathBuf::from(d).join("TuneServer"))
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join("tune-crash.log");
            let _ = std::fs::write(&log_path, &msg);
            default_hook(info);
        }));
    }

    eprintln!("tune-server starting (pid {})", std::process::id());

    #[cfg(windows)]
    {
        let log_dir = std::env::var("LOCALAPPDATA")
            .map(|d| std::path::PathBuf::from(d).join("TuneServer"))
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
        let _ = std::fs::create_dir_all(&log_dir);
        let startup_log = log_dir.join("tune-startup.log");
        let _ = std::fs::write(
            &startup_log,
            format!(
                "tune-server {} starting\npid: {}\nexe: {:?}\ncwd: {:?}\n",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                std::env::current_exe().ok(),
                std::env::current_dir().ok(),
            ),
        );
    }

    // On Windows, detect Program Files installs and migrate data to %LOCALAPPDATA%
    #[cfg(target_os = "windows")]
    crate::windows_migrate::check_and_migrate();

    // Load .env file if present (compatible with the Python server's .env convention).
    // dotenvy injects variables from .env into the process environment so that
    // TuneConfig::load() picks them up via std::env::var().  Missing .env is fine.
    //
    // Search order:
    //   1. CWD and ancestors (dotenvy::dotenv default)
    //   2. [Windows] %LOCALAPPDATA%\TuneServer\.env
    //   3. [Windows] directory containing tune-server.exe
    let mut dotenv_loaded = false;
    match dotenvy::dotenv() {
        Ok(path) => {
            eprintln!("loaded .env from {}", path.display());
            dotenv_loaded = true;
        }
        Err(dotenvy::Error::Io(_)) => {} // no .env file in CWD — try other locations
        Err(e) => eprintln!("warning: .env parse error: {e}"),
    }
    #[cfg(target_os = "windows")]
    if !dotenv_loaded {
        let extra_paths: Vec<std::path::PathBuf> = [
            std::env::var("LOCALAPPDATA")
                .ok()
                .map(|d| std::path::PathBuf::from(d).join("TuneServer").join(".env")),
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join(".env"))),
        ]
        .into_iter()
        .flatten()
        .collect();
        for path in &extra_paths {
            if path.is_file() {
                match dotenvy::from_path(path) {
                    Ok(()) => {
                        eprintln!("loaded .env from {}", path.display());
                        dotenv_loaded = true;
                        break;
                    }
                    Err(e) => eprintln!("warning: .env parse error at {}: {e}", path.display()),
                }
            }
        }
    }
    let _ = dotenv_loaded; // suppress unused warning on non-Windows

    // Install rustls CryptoProvider before any TLS operation (reqwest, etc.)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    let config = TuneConfig::load();

    // Use local time for log timestamps (fixes UTC display on Windows/CEST systems).
    // Must capture offset before spawning threads (security restriction on some OS).
    let time_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let time_fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory]:[offset_minute]"
    );
    let timer = tracing_subscriber::fmt::time::OffsetTime::new(time_offset, time_fmt);

    let env_filter = EnvFilter::from_default_env()
        .add_directive(format!("tune_server={}", config.log_level).parse().unwrap())
        .add_directive(format!("tune_core={}", config.log_level).parse().unwrap())
        // Cap chatty dependencies so a `debug` level (config or RUST_LOG=debug)
        // doesn't drown the useful lines. At debug, sqlx::query logs every SQL
        // statement and reqwest/hyper log every outbound connection: Elie's
        // 1000-line "Export logs" covered barely 7 seconds, ~95% of it sqlx +
        // reqwest::connect noise, burying the playback events we actually needed.
        // These crates are never useful for diagnosing Tune. Target-specific
        // directives win over the global level, so this holds even at RUST_LOG=debug.
        .add_directive("sqlx=warn".parse().unwrap())
        .add_directive("reqwest=info".parse().unwrap())
        .add_directive("hyper=info".parse().unwrap())
        .add_directive("hyper_util=info".parse().unwrap())
        .add_directive("h2=info".parse().unwrap())
        .add_directive("rustls=info".parse().unwrap())
        .add_directive("mio=info".parse().unwrap());

    // Write logs to a file on every platform (Linux included) so the
    // Diagnostics "Export logs" button and /system/logs work even when not
    // launched from a terminal — systemd/journald, Docker, or a double-clicked
    // .app. The path is shared with the reader via config::default_log_file_path()
    // so both always agree. Previously Linux wrote no file, so any launch where
    // journalctl didn't apply exported an empty log.
    // Plafond du journal : 10 Mio pour le fichier courant, plus une sauvegarde
    // `.1` — soit ~2× sur le disque.
    //
    // Il est tenu à DEUX moments, et il faut les deux. `rotate_log_file` range
    // au démarrage ce que la session précédente a laissé ; `JournalBorne` tient
    // le plafond *pendant* que le serveur tourne. Jusqu'ici seul le premier
    // existait, et #539 l'assumait — mais un serveur qui tourne longtemps est
    // justement le seul qui puisse dépasser 10 Mio (voir tune-server/journal.rs
    // et #2156).
    const PLAFOND_JOURNAL: u64 = 10 * 1024 * 1024;
    let log_file = {
        let path = config::default_log_file_path();
        config::rotate_log_file(&path, PLAFOND_JOURNAL);
        crate::journal::JournalBorne::ouvrir(path.clone(), PLAFOND_JOURNAL)
            .ok()
            .map(|j| {
                eprintln!("Logging to {}", path.display());
                j
            })
    };

    if let Some(file) = log_file {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let file_timer = tracing_subscriber::fmt::time::OffsetTime::new(time_offset, time_fmt);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_timer(file_timer)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file));
        let stderr_layer = tracing_subscriber::fmt::layer().with_timer(timer);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_timer(timer)
            .with_env_filter(env_filter)
            .init();
    }

    // Image builders alone cannot protect appliances already in the field:
    // self-update replaces this binary, not /etc.  On Tune OS/Linux, migrate
    // the historical public SSH password once before exposing the HTTP API.
    // The embedded policy preserves every password that was already changed.
    #[cfg(target_os = "linux")]
    crate::tune_os_password::migrate_legacy_password();

    // Bind the HTTP listener BEFORE opening the database. If another
    // tune-server instance is already running (old LaunchAgent, manual
    // install, update race — Jean-Marie/FRIDER #1158), the previous order
    // opened + migrated the shared DB to the new schema, then died on the
    // bind failure — leaving the old binary serving a database it no longer
    // understood (tags "lost", albums split, Next broken). Failing fast on
    // the port keeps the DB untouched. Connections arriving before the
    // router is up simply queue in the backlog.
    // Écoute en double pile quand la machine le permet, IPv4 seule sinon :
    // Firefox frappe `[::1]` pour `localhost` et recevait « connexion refusée »
    // sur une socket IPv4 seule (#1321). Le repli couvre les machines où IPv6
    // est désactivé, et la reprise de port ci-dessous reste inchangée.
    let v4_addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let (listener, boot_socket) = {
        let (mut socket, mut addr) = crate::config::dual_stack_listen_socket(config.port)
            .unwrap_or_else(|| (crate::config::ipv4_listen_socket(), v4_addr));
        let mut ipv6_attempted = addr.is_ipv6();
        #[cfg(unix)]
        let mut reclaim_tried = false;
        for attempt in 1..=10u32 {
            match socket.bind(&addr.into()) {
                Ok(()) => break,
                // Premier échec sur la socket IPv6 : la pile est peut-être
                // désactivée sur la machine. On repasse en IPv4 seule plutôt
                // que d'épuiser les tentatives puis de sortir en erreur.
                Err(e) if ipv6_attempted => {
                    tracing::info!(error = %e, "bind IPv6 impossible, repli sur IPv4 seule");
                    ipv6_attempted = false;
                    socket = crate::config::ipv4_listen_socket();
                    addr = v4_addr;
                    continue;
                }
                Err(e) if attempt < 10 => {
                    tracing::warn!(%addr, attempt, error = %e, "bind failed, retrying in 2s");
                    // The port is held by another process. If it is a *stale*
                    // tune-server instance (an old build that wasn't stopped
                    // before this launch / in-app update — Vincent's macOS
                    // dual-instance "boucle", #1158), the updater re-execs the
                    // new binary but never tells the previous separate process
                    // to quit, so two servers keep controlling the renderer and
                    // the track restarts every few seconds. Reclaim the port
                    // from that stale sibling exactly once so the freshly
                    // launched/updated binary wins ("last launch wins"), instead
                    // of exiting and leaving the old one alive. Only a process
                    // that (a) is bound to *our* port and (b) is itself a
                    // tune-server is ever signalled — never an unrelated
                    // process, never a tune-server on a different port.
                    #[cfg(unix)]
                    if !reclaim_tried {
                        reclaim_tried = true;
                        reclaim_port_from_stale_instance(config.port);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                Err(e) => {
                    // Another tune-server is already listening on this port
                    // (e.g. an old instance that wasn't stopped before an
                    // update/restart — Elie). Exit cleanly with an actionable
                    // message instead of panicking, which dumped core and
                    // spammed the journal on every restart of the crash loop.
                    tracing::error!(
                        %addr,
                        error = %e,
                        "failed to bind after 10 attempts — another tune-server \
                         instance is probably already bound to this port. Stop \
                         it before starting a new one \
                         (e.g. `systemctl stop tune-server` or `pkill -f tune-server`)."
                    );
                    std::process::exit(1);
                }
            }
        }
        socket.listen(128).expect("failed to listen");
        socket
            .set_nonblocking(true)
            .expect("failed to set nonblocking");
        // Descripteur dupliqué de la MÊME socket, pour le répondeur de
        // démarrage (#1701) : il accepte les connexions pendant que la base se
        // met à niveau, puis rend la place à axum. Dupliquer plutôt que
        // réécouter garde la protection ci-dessus (un seul serveur tient le
        // port) et le backlog déjà constitué.
        let boot_socket = socket.try_clone().ok().map(std::net::TcpListener::from);
        (
            tokio::net::TcpListener::from_std(socket.into()).expect("failed to create listener"),
            boot_socket,
        )
    };
    // Adresse réellement obtenue : `[::]` en double pile, `0.0.0.0` en repli.
    let addr = listener.local_addr().unwrap_or(v4_addr);

    // À partir d'ici et jusqu'à ce qu'axum serve, quelqu'un répond. Sans ça,
    // une migration longue laissait le navigateur tourner dans le vide : le
    // testeur « eric » a signalé « l'installation de la 9.70 plante » alors que
    // la base se mettait à niveau, en silence (#1701, fil forum 1386).
    let boot_responder = boot_socket.map(crate::boot_status::spawn);

    // Appliance : ne jamais démarrer sur une base vide si le disque de
    // données externe est absent (docs/DATA-RELOCATION.md).
    crate::boot_status::set_phase("attente du disque de données");
    crate::routes::appliance_storage::wait_for_data_volume(&config.db_path).await;

    crate::boot_status::set_phase("base de données");
    let state = AppState::new(&config.db_path, config.port, config.clone())
        .expect("failed to init app state");

    crate::boot_status::set_phase("configuration");
    state.restore_tokens().await;

    // Restore zone volumes, persist music_dirs/discogs_token to DB
    crate::startup::init_state(&state, &config).await;

    // Record initial server_last_alive_at for auto-resume crash detection
    {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        settings.set("server_last_alive_at", &now.to_string()).ok();
    }

    // Auto-scan music directories at startup
    let scan_done = if config.auto_scan {
        Some(crate::auto_scan::spawn_auto_scan(
            state.backend.clone(),
            state.event_bus.clone(),
        ))
    } else {
        None
    };

    // File watcher for live directory changes (waits for auto-scan to finish
    // before monitoring, to avoid racing with the scanner on macOS FSEvents)
    crate::auto_scan::spawn_file_watcher(state.backend.clone(), scan_done, state.event_bus.clone());

    // Remonter les partages reseau AVANT toute lecture de la bibliotheque : un
    // partage absent fait voir un repertoire vide, et le scan qui suit conclut
    // « 0 fichier » (#1692).
    crate::boot_status::set_phase("partages réseau");
    crate::startup::remount_network_shares(&state).await;

    // Register local audio outputs (USB DAC, headphones, speakers)
    crate::boot_status::set_phase("sorties audio");
    #[cfg(feature = "local-audio")]
    crate::startup::register_local_outputs(&state).await;

    // Plugins. After local outputs so a plugin output never races the
    // local-device scan for the same zone row; before `routes::router`, which
    // needs the routers plugins contribute.
    //
    // `build_plugins` is where an out-of-tree binary injects its own. It runs
    // here, and not earlier, because a plugin's host services (`services`,
    // `backend`, `http_client`) only exist once `state` does.
    crate::boot_status::set_phase("greffons");
    let extra_plugins = build_plugins.map(|build| build(&state)).unwrap_or_default();
    let plugin_routers = crate::plugins::init(
        &state,
        &format!("http://127.0.0.1:{}", config.port),
        extra_plugins,
    )
    .await;

    // P2 of the plugin ABI: load enabled wasm plugins into AppState so the
    // `/api/v1/plugins/{id}/…` mount can dispatch into them. Feature-gated and
    // fail-safe (a bad plugin is skipped, never fatal). See run.rs.
    #[cfg(feature = "plugins-wasm")]
    crate::plugins_host::load_wasm_plugins(&state).await;
    // P3 of the plugin ABI (RFC §3.6): fan `event_bus` events out to subscribed
    // wasm plugins' `plugin_on_event`. Background task; fail-safe (a plugin can
    // never break the bus). Must run after the registry is published above.
    #[cfg(feature = "plugins-wasm")]
    crate::plugins_host::spawn_wasm_event_forwarder(&state);

    // NOTE: local-zone auto-resume is deferred until AFTER the HTTP listener is
    // bound (see below). Running it here fetched the local output's own
    // /stream/ URL before the server was accepting connections, which failed
    // with local_audio_http_fetch_failed and left playback silently dead.

    // Create shared OpenHome event listener
    crate::boot_status::set_phase("découverte réseau");
    let oh_event_listener = crate::startup::create_oh_listener().await;

    // SSDP discovery (DLNA / OpenHome)
    crate::discovery_setup::spawn_ssdp_handler(&state, &config, oh_event_listener);

    // mDNS discovery (Chromecast, AirPlay, BluOS, OAAT, Squeezebox)
    let _mdns_handle = crate::discovery_setup::spawn_mdns_handler(&state);

    // Sorties hors-arbre apportees par un binaire composeur (tune-diretta).
    // Sans effet pour le binaire standard : `RunOptions::default()` n'a aucun
    // fournisseur. C'est l'appel dont la disparition a casse l'integration
    // partenaire pendant deux versions (#1510).
    crate::discovery_setup::spawn_output_providers(&state, opts.output_providers);

    // Background tasks: squeezebox poller, session GC, position poller,
    // token refresh, UPnP advertiser, Deezer proxy, alarms, notifications, memory diag
    crate::background::spawn_background_tasks(&state, &config).await;

    // Auto-resume network zones (waits for device.reconnected events)
    crate::auto_resume::spawn_auto_resume_listener(&state);

    state.event_bus.emit(
        "system.started",
        serde_json::json!({
            "version": tune_core::version(),
            "port": config.port,
        }),
    );

    info!(
        version = tune_core::version(),
        port = config.port,
        db = %config.db_path,
        web = %crate::config::resolve_web_dir().display(),
        "tune_server_starting"
    );

    routes::spotify_connect::auto_start(&state).await;

    // Clone before `state` is moved into the router — used to auto-resume local
    // zones once the listener is bound (see below).
    #[cfg(feature = "local-audio")]
    let resume_state = state.clone();

    // Kept for the shutdown hook — `state` is moved into the router below.
    let plugins_handle = state.plugins.clone();

    // Cloné AVANT que le routeur ne consomme `state` : l'arrêt propre en a
    // besoin pour replier le WAL.
    let shutdown_state = state.clone();
    let app = routes::router_with_plugins(state, plugin_routers);

    // Le routeur est prêt : le répondeur de démarrage rend la socket. `stop()`
    // attend la sortie du fil, donc il n'y a jamais deux accepteurs à la fois
    // et aucune connexion ne peut recevoir un « je démarre » après coup.
    if let Some(responder) = boot_responder {
        responder.stop();
    }

    // Listener was bound before the DB was opened (see above) — the socket's
    // backlog has been queueing connections since then.
    info!(%addr, "listening");

    // Dire l'adresse COMPLÈTE, une fois, là où quelqu'un la lit (#1272) :
    // fenêtre de console Windows ouverte par `start-tune-server.bat`,
    // `docker logs`, `journalctl`. Elle n'était imprimée nulle part — ni ici,
    // ni par un installeur — alors que le serveur la calcule déjà pour
    // l'interface. Voir [`crate::adresse_d_accueil`] pour ce que cela ne
    // règle PAS : une adresse sans port a besoin du port 80.
    for ligne in crate::adresse_d_accueil::lignes_d_accueil(
        config.port,
        &routes::system::server_urls(config.port),
    ) {
        eprintln!("{ligne}");
        info!(%ligne, "adresse_d_accueil");
    }

    // Auto-resume local zones now that the listener is bound. Wait until the
    // server is actually accepting connections before resuming, so the local
    // output can fetch its own /stream/ URL (fixes the startup race that caused
    // local_audio_http_fetch_failed → silent no-playback on ASIO).
    #[cfg(feature = "local-audio")]
    {
        let resume_port = config.port;
        tokio::spawn(async move {
            for _ in 0..20 {
                if tokio::net::TcpStream::connect(format!("127.0.0.1:{resume_port}"))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            crate::auto_resume::auto_resume_local_zones(&resume_state).await;
        });
    }

    // Open browser after listener is bound (server is ready to accept connections).
    // Only when TUNE_OPEN_BROWSER=1 — set by launcher scripts (start-tune-server.bat/.command).
    if std::env::var("TUNE_OPEN_BROWSER").ok().as_deref() == Some("1") {
        let port = config.port;
        tokio::spawn(async move {
            // Wait until the server is actually accepting connections before opening the browser.
            // Poll via TCP connect every 500ms, up to 10 attempts (5s max).
            for attempt in 1..=10 {
                if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                    .await
                    .is_ok()
                {
                    info!(attempt, "server_ready_for_browser");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let url = format!("http://localhost:{port}");
            info!(url = %url, "opening_browser");
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("open").arg(&url).spawn();
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", &url])
                .spawn();
            #[cfg(target_os = "linux")]
            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        });
    }

    if let Err(e) = axum::serve(
        listener,
        // ConnectInfo<SocketAddr> lets handlers see the client IP (used to
        // disambiguate browser zones created by different machines — Bertrand).
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_state))
    .await
    {
        tracing::error!(error = %e, "server_fatal_error");
        #[cfg(windows)]
        {
            let log_dir = std::env::var("LOCALAPPDATA")
                .map(|d| std::path::PathBuf::from(d).join("TuneServer"))
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
            let _ = std::fs::create_dir_all(&log_dir);
            let _ = std::fs::write(log_dir.join("tune-crash.log"), format!("SERVER ERROR: {e}"));
        }
    }

    // Graceful shutdown returned — let plugins flush and close before the
    // process exits. `shutdown_signal` already arms a 3s hard-exit timer, so
    // a plugin that hangs here cannot wedge the shutdown.
    crate::plugins::shutdown(&plugins_handle).await;
}

async fn shutdown_signal(state: crate::state::AppState) {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await.expect("failed to install CTRL+C handler");

    info!("shutdown_signal_received");

    // Replier le WAL tout de suite, avant le reste de l'arrêt. Quand
    // onnxruntime est chargé, l'arrêt peut se solder par un SEGV pendant le
    // démontage (#1462) : le processus meurt alors sans que rien ne soit
    // rabattu. En repliant ici, ce qui suit ne peut plus laisser de WAL à
    // rejouer, quelle que soit la façon dont le processus se termine.
    if let Some(db) = state.db.as_ref() {
        db.checkpoint();
    }

    // Force exit after 3s if graceful shutdown stalls — must use std::thread
    // because tokio runtime may itself be stalling
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        tracing::warn!("shutdown_timeout_forcing_exit");
        // `_exit`, PAS `std::process::exit`. Cette minuterie s'arme quand
        // l'arrêt propre a déjà échoué : d'autres fils tournent encore, par
        // construction. `exit()` déroule alors `__run_exit_handlers` →
        // `_dl_fini`, donc les destructeurs statiques de TOUTES les
        // bibliothèques chargées — dont `libonnxruntime.so`, dont les fils
        // d'inférence et les arènes sont toujours vivants. Il y meurt.
        //
        // Prouvé sur .18 le 11 août (#1462), pile extraite du core :
        //   #0  libonnxruntime.so
        //   #3  _dl_call_fini      (dl-call_fini.c:43)
        //   #4  _dl_fini
        //   #5  __run_exit_handlers
        //   #6  __GI_exit
        // et dans le journal, `shutdown_signal_received` puis, exactement
        // 3,001 s plus tard, `shutdown_timeout_forcing_exit` suivi de
        // `status=11/SEGV`. C'est notre propre garde-fou qui tuait le
        // processus — et un SEGV au milieu de `_dl_fini`, pendant que des
        // fils écrivent encore, est autrement plus dangereux pour la base
        // qu'un arrêt franc (lequel, lui, n'a rien corrompu le 11 août).
        //
        // `_exit` rend la main au noyau sans dérouler quoi que ce soit. C'est
        // exactement ce qu'on veut d'une sortie forcée : on a déjà renoncé au
        // nettoyage, il ne reste qu'à ne pas faire de dégâts.
        unsafe { libc::_exit(0) };
    });
}

/// Terminate a *stale* tune-server instance that is holding `port`, so a newly
/// launched or freshly-updated binary can bind and take over.
///
/// This runs only when our own `bind()` has already failed, i.e. the port is
/// genuinely contended — a normal startup with a free port never signals
/// anything. It is deliberately surgical to avoid the danger of killing the
/// wrong process:
///
///   1. `lsof` tells us the exact PID(s) *listening* on our port.
///   2. We skip our own PID.
///   3. We only signal a PID whose executable base name matches ours — an
///      unrelated program that happens to hold the port (or a tune-server
///      bound to a *different* port) is never touched. If the holder can't be
///      confirmed as a tune-server we leave it alone and let the caller's
///      bind-retry / exit(1) guard handle it (protecting the shared DB).
///
/// SIGTERM is sent first (graceful), with SIGKILL as a backstop so the port is
/// reliably freed even if the old instance is wedged in its dlna_play loop.
///
/// Trade-off: the historical behaviour was "first launch wins" — a second
/// instance failed to bind and exited (main.rs bind guard), which protected the
/// DB but is wrong for an update, where the *new* binary must supersede the old
/// one. Reclaiming the contended port makes it "last launch wins" for that one
/// port only, which is exactly what an in-app update needs, while keeping the
/// exit(1) guard as a backstop for the case where the holder is not a
/// tune-server we can safely stop.
#[cfg(unix)]
fn reclaim_port_from_stale_instance(port: u16) {
    let self_pid = std::process::id();

    let own_name = match std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    {
        Some(n) if !n.is_empty() => n,
        _ => {
            tracing::warn!(
                "could not determine own executable name; skipping stale-instance cleanup"
            );
            return;
        }
    };

    let listeners = pids_listening_on(port);
    if listeners.is_empty() {
        // lsof missing or nothing detected — leave the bind guard to handle it.
        return;
    }

    let mut targets: Vec<u32> = Vec::new();
    for pid in listeners {
        if pid == self_pid {
            continue;
        }
        match process_base_name(pid) {
            Some(name) if same_executable(&name, &own_name) => targets.push(pid),
            Some(name) => tracing::warn!(
                pid,
                port,
                holder = %name,
                "port held by a non-tune-server process — not signalling it"
            ),
            None => tracing::warn!(
                pid,
                port,
                "could not identify port holder — not signalling it"
            ),
        }
    }

    if targets.is_empty() {
        return;
    }

    tracing::warn!(
        ?targets,
        port,
        "reclaiming port from stale tune-server instance(s) so the new binary can bind (last-launch-wins)"
    );
    for pid in &targets {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    // Give the old instance a moment to release the socket gracefully, then
    // force-kill anything that ignored SIGTERM so bind() can succeed on retry.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    for pid in &targets {
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// PIDs listening on `port` (TCP), via `lsof`. Empty on any failure.
#[cfg(unix)]
fn pids_listening_on(port: u16) -> Vec<u32> {
    // `-iTCP:<port>` + `-sTCP:LISTEN` selects only the process listening on that
    // TCP port; `-t` prints bare PIDs.
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-sTCP:LISTEN"])
        .arg(format!("-iTCP:{port}"))
        .arg("-t")
        .output();
    let output = match output {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

/// Executable base name of `pid` (e.g. `tune-server`), via `ps`. `None` on
/// failure. macOS exposes the basename as `ucomm`, Linux as `comm`.
#[cfg(unix)]
fn process_base_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    let field = "ucomm=";
    #[cfg(not(target_os = "macos"))]
    let field = "comm=";

    let output = std::process::Command::new("ps")
        .args(["-o", field, "-p"])
        .arg(pid.to_string())
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&output.stdout);
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    // `comm`/`ucomm` may still be a full path on some platforms — reduce to the
    // final path component.
    let base = std::path::Path::new(line)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| line.to_string());
    Some(base)
}

/// Whether two executable base names refer to the same binary, tolerating the
/// 15/16-char truncation that `ps` applies to the accounting name.
#[cfg(unix)]
fn same_executable(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // One side may be truncated by ps (TASK_COMM_LEN / MAXCOMLEN).
    (a.len() >= 15 && b.starts_with(a)) || (b.len() >= 15 && a.starts_with(b))
}

/// Les arguments demandent-ils la version ?
///
/// Extrait de [`run_with`] pour être testable : la branche appelante quitte le
/// processus, ce qu'un test ne peut pas observer. L'appelant a déjà écarté
/// `argv[0]`.
///
/// Comparaison stricte, jamais un préfixe : `--verbose` ne doit pas faire
/// sortir un serveur au démarrage.
fn version_requested<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == "--version" || a == "-V")
}

#[cfg(test)]
mod tests {
    use super::version_requested;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn les_deux_formes_demandent_la_version() {
        assert!(version_requested(args(&["--version"])));
        assert!(version_requested(args(&["-V"])));
    }

    #[test]
    fn un_demarrage_ordinaire_ne_demande_rien() {
        assert!(!version_requested(args(&[])));
        assert!(!version_requested(args(&[
            "--config",
            "/opt/tune/tune.toml"
        ])));
    }

    /// Le vrai risque de ce drapeau : un serveur qui sort au lieu de démarrer.
    /// Un préfixe ne doit JAMAIS suffire.
    #[test]
    fn un_argument_qui_commence_pareil_ne_fait_pas_sortir_le_serveur() {
        assert!(!version_requested(args(&["--verbose"])));
        assert!(!version_requested(args(&["--version-check"])));
        assert!(!version_requested(args(&["-Version"])));
        assert!(!version_requested(args(&["-Vv"])));
    }

    /// La sonde wasm est dispatchée avant, mais si l'ordre changeait un jour,
    /// ce test rappelle que ses arguments ne doivent pas déclencher la sortie.
    #[test]
    fn les_arguments_de_la_sonde_wasm_ne_declenchent_rien() {
        assert!(!version_requested(args(&["--wasm-probe", "/tmp/x.wasm"])));
    }
}
