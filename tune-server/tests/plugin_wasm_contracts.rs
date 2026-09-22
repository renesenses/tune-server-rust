//! Contrats WASM sérialisés autour de `TUNE_PLUGINS_DIR`.

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_environment() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// #4717 — le greffon « Playlists converter » de bout en bout. Il REJOINT cette
// cible plutôt que d'en ouvrir une : comme les autres, il pointe
// `TUNE_PLUGINS_DIR` sur le dossier de greffons commité, et cette variable est
// globale au processus. Une cible à part la ferait basculer sous les pieds des
// essais voisins — c'est précisément ce que `lock_environment` sérialise.
#[path = "greffon_playlists_converter_4717.rs"]
mod greffon_playlists_converter_4717;
#[path = "plugin_events.rs"]
mod plugin_events;
#[path = "plugin_party_e2e.rs"]
mod plugin_party_e2e;
#[path = "plugin_uninstall_4194.rs"]
mod plugin_uninstall_4194;
#[path = "plugin_wasm_routes.rs"]
mod plugin_wasm_routes;
