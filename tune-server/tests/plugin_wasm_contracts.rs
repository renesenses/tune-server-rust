//! Contrats WASM sérialisés autour de `TUNE_PLUGINS_DIR`.

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_environment() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[path = "plugin_events.rs"]
mod plugin_events;
#[path = "plugin_party_e2e.rs"]
mod plugin_party_e2e;
#[path = "plugin_wasm_routes.rs"]
mod plugin_wasm_routes;
