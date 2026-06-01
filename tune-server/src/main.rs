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
            let msg = format!("PANIC: {info}");
            eprintln!("{msg}");
            let log_path = std::env::current_dir()
                .unwrap_or_default()
                .join("tune-crash.log");
            let _ = std::fs::write(&log_path, &msg);
            default_hook(info);
        }));
    }

    eprintln!("tune-server starting (pid {})", std::process::id());

    // On Windows, detect Program Files installs and migrate data to %LOCALAPPDATA%
    #[cfg(target_os = "windows")]
    tune_server::windows_migrate::check_and_migrate();

    // Install rustls CryptoProvider before any TLS operation (reqwest, etc.)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    let config = TuneConfig::load();

    // Use local time for log timestamps (fixes UTC display on Windows/CEST systems).
    // Must capture offset before spawning threads (security restriction on some OS).
    let time_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let timer = tracing_subscriber::fmt::time::OffsetTime::new(
        time_offset,
        time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory]:[offset_minute]"
        ),
    );

    tracing_subscriber::fmt()
        .with_timer(timer)
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(format!("tune_server={}", config.log_level).parse().unwrap())
                .add_directive(format!("tune_core={}", config.log_level).parse().unwrap()),
        )
        .init();

    let state = AppState::new(&config.db_path, config.port, config.clone())
        .expect("failed to init app state");

    state.restore_tokens().await;

    // Restore zone volumes, persist music_dirs/discogs_token to DB
    tune_server::startup::init_state(&state, &config).await;

    // Auto-scan music directories at startup
    if config.auto_scan {
        tune_server::auto_scan::spawn_auto_scan(state.db.clone(), state.event_bus.clone());
    }

    // File watcher for live directory changes
    tune_server::auto_scan::spawn_file_watcher(state.db.clone());

    // Register local audio outputs (USB DAC, headphones, speakers)
    #[cfg(feature = "local-audio")]
    tune_server::startup::register_local_outputs(&state).await;

    // Create shared OpenHome event listener
    let oh_event_listener = tune_server::startup::create_oh_listener().await;

    // SSDP discovery (DLNA / OpenHome)
    tune_server::discovery_setup::spawn_ssdp_handler(&state, &config, oh_event_listener);

    // mDNS discovery (Chromecast, AirPlay, BluOS, OAAT, Squeezebox)
    let _mdns_handle = tune_server::discovery_setup::spawn_mdns_handler(&state);

    // Background tasks: squeezebox poller, session GC, position poller,
    // token refresh, UPnP advertiser, Deezer proxy, alarms, notifications, memory diag
    tune_server::background::spawn_background_tasks(&state, &config).await;

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
        web = %config.web_dir,
        "tune_server_starting"
    );

    routes::spotify_connect::auto_start(&state).await;

    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                tracing::warn!(addr = %addr, error = %e, "port_busy_retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };

    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
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
        eprintln!("shutdown_timeout_forcing_exit");
        std::process::exit(0);
    });
}
