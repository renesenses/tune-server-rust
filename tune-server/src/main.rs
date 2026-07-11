use tune_server::config;
use tune_server::routes;
use tune_server::state;

use std::net::SocketAddr;

use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::TuneConfig;
use crate::state::AppState;

#[tokio::main]
async fn main() {
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
    tune_server::windows_migrate::check_and_migrate();

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
        .add_directive(format!("tune_core={}", config.log_level).parse().unwrap());

    // On macOS/Windows, also write logs to a file so the Diagnostics
    // "Export logs" button works even when not launched from a terminal.
    let log_file = if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        let log_dir = if cfg!(target_os = "macos") {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join("Library/Logs"))
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
        } else {
            std::env::var("LOCALAPPDATA")
                .map(|d| std::path::PathBuf::from(d).join("TuneServer"))
                .unwrap_or_else(|_| std::path::PathBuf::from("C:\\ProgramData\\TuneServer"))
        };
        std::fs::create_dir_all(&log_dir).ok();
        let path = log_dir.join("tune-server.log");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| {
                eprintln!("Logging to {}", path.display());
                f
            })
    } else {
        None
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

    let state = AppState::new(&config.db_path, config.port, config.clone())
        .expect("failed to init app state");

    state.restore_tokens().await;

    // Restore zone volumes, persist music_dirs/discogs_token to DB
    tune_server::startup::init_state(&state, &config).await;

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
        Some(tune_server::auto_scan::spawn_auto_scan(
            state.backend.clone(),
            state.event_bus.clone(),
        ))
    } else {
        None
    };

    // File watcher for live directory changes (waits for auto-scan to finish
    // before monitoring, to avoid racing with the scanner on macOS FSEvents)
    tune_server::auto_scan::spawn_file_watcher(state.backend.clone(), scan_done);

    // Register local audio outputs (USB DAC, headphones, speakers)
    #[cfg(feature = "local-audio")]
    tune_server::startup::register_local_outputs(&state).await;

    // NOTE: local-zone auto-resume is deferred until AFTER the HTTP listener is
    // bound (see below). Running it here fetched the local output's own
    // /stream/ URL before the server was accepting connections, which failed
    // with local_audio_http_fetch_failed and left playback silently dead.

    // Create shared OpenHome event listener
    let oh_event_listener = tune_server::startup::create_oh_listener().await;

    // SSDP discovery (DLNA / OpenHome)
    tune_server::discovery_setup::spawn_ssdp_handler(&state, &config, oh_event_listener);

    // mDNS discovery (Chromecast, AirPlay, BluOS, OAAT, Squeezebox)
    let _mdns_handle = tune_server::discovery_setup::spawn_mdns_handler(&state);

    // Background tasks: squeezebox poller, session GC, position poller,
    // token refresh, UPnP advertiser, Deezer proxy, alarms, notifications, memory diag
    tune_server::background::spawn_background_tasks(&state, &config).await;

    // Auto-resume network zones (waits for device.reconnected events)
    tune_server::auto_resume::spawn_auto_resume_listener(&state);

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

    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("failed to create socket");
        socket.set_reuse_address(true).ok();
        for attempt in 1..=10u32 {
            match socket.bind(&addr.into()) {
                Ok(()) => break,
                Err(e) if attempt < 10 => {
                    tracing::warn!(%addr, attempt, error = %e, "bind failed, retrying in 2s");
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
        tokio::net::TcpListener::from_std(socket.into()).expect("failed to create listener")
    };

    info!(%addr, "listening");

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
            tune_server::auto_resume::auto_resume_local_zones(&resume_state).await;
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

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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
}

async fn shutdown_signal() {
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

    // Force exit after 3s if graceful shutdown stalls — must use std::thread
    // because tokio runtime may itself be stalling
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        tracing::warn!("shutdown_timeout_forcing_exit");
        std::process::exit(0);
    });
}
