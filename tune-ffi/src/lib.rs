#![allow(unsafe_op_in_unsafe_fn)]
//! FFI bridge for embedding Tune Server in mobile apps (Flutter/Android, iOS).
//!
//! Exposes a minimal C API:
//! - `tune_server_start(port, db_path, music_dirs, web_dir)` → starts the server
//! - `tune_server_stop()` → gracefully stops the server
//! - `tune_server_status()` → returns JSON status string
//! - `tune_server_version()` → returns version string
//! - `tune_free_string(ptr)` → frees a string returned by this library

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::runtime::Runtime;
use tracing::info;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_TX: OnceLock<tokio::sync::watch::Sender<bool>> = OnceLock::new();

fn get_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name("tune-ffi")
            .build()
            .expect("failed to create tokio runtime")
    })
}

/// Start the Tune server on the given port.
///
/// # Arguments
/// - `port` — HTTP port (e.g. 8888)
/// - `db_path` — path to the SQLite database file
/// - `music_dirs_json` — JSON array of music directory paths, e.g. `["/sdcard/Music"]`
/// - `web_dir` — path to the web client assets directory (or null to skip)
///
/// Returns 0 on success, -1 if already running, -2 on error.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_start(
    port: u16,
    db_path: *const c_char,
    music_dirs_json: *const c_char,
    web_dir: *const c_char,
) -> i32 {
    if RUNNING.load(Ordering::SeqCst) {
        return -1; // already running
    }

    let db_path = unsafe { CStr::from_ptr(db_path) }
        .to_str()
        .unwrap_or("tune.db")
        .to_string();

    let music_dirs: Vec<String> = if music_dirs_json.is_null() {
        vec![]
    } else {
        let json_str = unsafe { CStr::from_ptr(music_dirs_json) }
            .to_str()
            .unwrap_or("[]");
        serde_json::from_str(json_str).unwrap_or_default()
    };

    let web_dir = if web_dir.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(web_dir) }
                .to_str()
                .unwrap_or("")
                .to_string(),
        )
    };

    // Initialize tracing (once)
    static TRACING_INIT: std::sync::Once = std::sync::Once::new();
    TRACING_INIT.call_once(|| {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,tune_core=info,tune_server=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let _ = SHUTDOWN_TX.set(shutdown_tx);

    let rt = get_runtime();

    rt.spawn(async move {
        RUNNING.store(true, Ordering::SeqCst);
        info!(port, db = %db_path, "tune_ffi_server_starting");

        match run_server(port, db_path, music_dirs, web_dir, shutdown_rx).await {
            Ok(()) => info!("tune_ffi_server_stopped"),
            Err(e) => tracing::error!(error = %e, "tune_ffi_server_error"),
        }

        RUNNING.store(false, Ordering::SeqCst);
    });

    0
}

/// Stop the Tune server gracefully.
/// Returns 0 on success, -1 if not running.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_stop() -> i32 {
    if !RUNNING.load(Ordering::SeqCst) {
        return -1;
    }
    if let Some(tx) = SHUTDOWN_TX.get() {
        let _ = tx.send(true);
    }
    0
}

/// Returns a JSON string with the server status.
/// Caller must free the returned string with `tune_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_status() -> *mut c_char {
    let running = RUNNING.load(Ordering::SeqCst);
    let json = serde_json::json!({
        "running": running,
        "version": tune_core::version(),
        "engine": "rust",
    });
    let s = CString::new(json.to_string()).unwrap_or_default();
    s.into_raw()
}

/// Returns the Tune server version string.
/// Caller must free the returned string with `tune_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_version() -> *mut c_char {
    let s = CString::new(tune_core::version()).unwrap_or_default();
    s.into_raw()
}

/// Free a string previously returned by this library.
#[unsafe(no_mangle)]
pub extern "C" fn tune_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal server runner
// ---------------------------------------------------------------------------

async fn run_server(
    port: u16,
    db_path: String,
    music_dirs: Vec<String>,
    web_dir: Option<String>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    use tune_server::config::TuneConfig;
    use tune_server::state::AppState;

    // Build config
    let mut config = TuneConfig::default();
    config.db_path = db_path;
    config.port = port;
    config.music_dirs = music_dirs;
    if let Some(ref wd) = web_dir {
        config.web_dir = wd.clone();
    }
    config.auto_scan = true;

    // Initialize state
    let state = AppState::new(&config.db_path, config.port, config.clone())
        .map_err(|e| format!("init state: {e}"))?;

    tune_server::startup::init_state(&state, &config).await;
    tune_server::startup::register_local_outputs(&state).await;

    let oh_listener = tune_server::startup::create_oh_listener().await;
    tune_server::discovery_setup::spawn_ssdp_handler(&state, &config, oh_listener);
    let _mdns = tune_server::discovery_setup::spawn_mdns_handler(&state);
    tune_server::background::spawn_background_tasks(&state, &config).await;

    // Build router
    let app = tune_server::routes::router(state);

    // Bind and serve
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;

    info!(%addr, "tune_ffi_listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
            info!("tune_ffi_shutdown_signal_received");
        })
        .await
        .map_err(|e| format!("serve: {e}"))?;

    Ok(())
}
