//! Contrats WASM sérialisés autour de `TUNE_PLUGINS_DIR`.

// Le verrou d'environnement est TENU pendant toute l'épreuve, `.await` compris :
// c'est lui qui sérialise les tests qui posent des variables d'environnement.
// `await_holding_lock` décrit ici l'intention même (clippy 1.98).
#![allow(clippy::await_holding_lock)]

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
#[path = "plugin_uninstall_4194.rs"]
mod plugin_uninstall_4194;
#[path = "plugin_wasm_routes.rs"]
mod plugin_wasm_routes;
