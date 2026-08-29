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
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::runtime::Runtime;
use tracing::info;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
// A `Mutex<Option<_>>` (not a `OnceLock`) so each start publishes a fresh
// sender and each stop clears it — otherwise a second start/stop cycle could
// never replace the sender and restart would be broken.
static SHUTDOWN_TX: Mutex<Option<tokio::sync::watch::Sender<bool>>> = Mutex::new(None);

/// Barriere d'ABI : une panique ne doit JAMAIS franchir un `extern "C"`.
///
/// Tant que le profil release portait `panic = "abort"`, la question ne se
/// posait pas — toute panique tuait le processus, ici comme ailleurs. En
/// repassant a `panic = "unwind"` (#2305), une panique qui traverse une
/// frontiere `extern "C"` fait avorter le processus : c'est la regle de Rust,
/// et c'est le pire des deux mondes pour l'hote Flutter ou Swift, qui perd
/// l'application entiere sans diagnostic.
///
/// Chaque entree convertit donc la panique en sa valeur d'echec documentee.
/// Le hook global de `bootstrap.rs` a deja ecrit `tune-crash.log` au moment de
/// la panique : on ne rejournalise pas la pile, seulement l'entree franchie.
fn barriere_abi<T>(entree: &'static str, valeur_si_panique: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(valeur) => valeur,
        Err(_) => {
            tracing::error!(entree, "tune_ffi_panique_interceptee");
            valeur_si_panique
        }
    }
}

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
    barriere_abi("tune_server_start", -2, || {
        demarrer(port, db_path, music_dirs_json, web_dir)
    })
}

fn demarrer(
    port: u16,
    db_path: *const c_char,
    music_dirs_json: *const c_char,
    web_dir: *const c_char,
) -> i32 {
    // Validate raw pointers before dereferencing: a null `db_path` would be UB
    // if passed to `CStr::from_ptr`, so reject it with the error code instead.
    if db_path.is_null() {
        return -2; // null db_path
    }

    // Claim the running slot atomically *before* spawning so two concurrent
    // starts can't both proceed. The server task clears the flag on exit, so a
    // later start (restart) can claim it again.
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
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
    // Publish a fresh sender for this run (replacing any stale one from a
    // previous start/stop cycle), so `tune_server_stop` can signal shutdown.
    if let Ok(mut guard) = SHUTDOWN_TX.lock() {
        *guard = Some(shutdown_tx);
    }

    let rt = get_runtime();

    rt.spawn(async move {
        // RUNNING was already claimed synchronously above.
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
    barriere_abi("tune_server_stop", -2, || {
        if !RUNNING.load(Ordering::SeqCst) {
            return -1;
        }
        // Take the sender so it is cleared for the next start/stop cycle.
        if let Ok(mut guard) = SHUTDOWN_TX.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(true);
            }
        }
        0
    })
}

/// Returns a JSON string with the server status.
/// Caller must free the returned string with `tune_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_status() -> *mut c_char {
    barriere_abi("tune_server_status", std::ptr::null_mut(), || {
        let running = RUNNING.load(Ordering::SeqCst);
        let json = serde_json::json!({
            "running": running,
            "version": tune_core::version(),
            "engine": "rust",
        });
        let s = CString::new(json.to_string()).unwrap_or_default();
        s.into_raw()
    })
}

/// Returns the Tune server version string.
/// Caller must free the returned string with `tune_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_version() -> *mut c_char {
    barriere_abi("tune_server_version", std::ptr::null_mut(), || {
        let s = CString::new(tune_core::version()).unwrap_or_default();
        s.into_raw()
    })
}

/// Free a string previously returned by this library.
///
/// # Contrat
/// `ptr` doit provenir de `tune_server_status` ou `tune_server_version`, et
/// n'etre libere qu'une fois. C'est un contrat d'API C : `unsafe` n'a aucune
/// signification pour l'appelant Swift ou Dart, et marquer la fonction
/// `unsafe extern "C"` changerait la signature vue des liaisons existantes sans
/// rien apporter cote hote. Le lint est donc leve ICI, avec sa raison, plutot
/// que globalement.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn tune_free_string(ptr: *mut c_char) {
    barriere_abi("tune_free_string", (), || {
        if !ptr.is_null() {
            unsafe {
                let _ = CString::from_raw(ptr);
            }
        }
    })
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
    #[cfg(feature = "local-audio")]
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // The FFI entry points touch process-global state (`RUNNING`), so the tests
    // that manipulate it are serialized to stay deterministic under the default
    // parallel test runner.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn null_db_path_is_rejected_without_panicking() {
        let _g = test_lock();
        // A null `db_path` must return the error code, not dereference the
        // pointer, and must not claim the running slot.
        let rc = tune_server_start(0, std::ptr::null(), std::ptr::null(), std::ptr::null());
        assert_eq!(rc, -2, "null db_path should return -2");
        assert!(
            !RUNNING.load(Ordering::SeqCst),
            "a rejected start must not mark the server running"
        );
    }

    #[test]
    fn double_start_is_rejected() {
        let _g = test_lock();
        // Simulate a server already running by claiming the slot, then a second
        // start must be rejected (-1) without spawning or binding anything.
        RUNNING.store(true, Ordering::SeqCst);
        let db = CString::new(":memory:").unwrap();
        let rc = tune_server_start(0, db.as_ptr(), std::ptr::null(), std::ptr::null());
        assert_eq!(rc, -1, "a second start should return -1 while running");
        // Restore global state for other tests.
        RUNNING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn stop_when_not_running_is_rejected() {
        let _g = test_lock();
        // Stopping a server that never started must return -1, not panic.
        RUNNING.store(false, Ordering::SeqCst);
        assert_eq!(tune_server_stop(), -1);
    }
}
