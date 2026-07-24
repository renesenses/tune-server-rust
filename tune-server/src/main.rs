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
    let log_file = {
        let path = config::default_log_file_path();
        // Cap the log at 10 MiB (keeping one .1 backup) so it doesn't grow
        // without bound on a long-running server.
        config::rotate_log_file(&path, 10 * 1024 * 1024);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| {
                eprintln!("Logging to {}", path.display());
                f
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

    // Bind the HTTP listener BEFORE opening the database. If another
    // tune-server instance is already running (old LaunchAgent, manual
    // install, update race — Jean-Marie/FRIDER #1158), the previous order
    // opened + migrated the shared DB to the new schema, then died on the
    // bind failure — leaving the old binary serving a database it no longer
    // understood (tags "lost", albums split, Next broken). Failing fast on
    // the port keeps the DB untouched. Connections arriving before the
    // router is up simply queue in the backlog.
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("failed to create socket");
        socket.set_reuse_address(true).ok();
        #[cfg(unix)]
        let mut reclaim_tried = false;
        for attempt in 1..=10u32 {
            match socket.bind(&addr.into()) {
                Ok(()) => break,
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
        tokio::net::TcpListener::from_std(socket.into()).expect("failed to create listener")
    };

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

    // Listener was bound before the DB was opened (see above) — the socket's
    // backlog has been queueing connections since then.
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

    if let Err(e) = axum::serve(
        listener,
        // ConnectInfo<SocketAddr> lets handlers see the client IP (used to
        // disambiguate browser zones created by different machines — Bertrand).
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
