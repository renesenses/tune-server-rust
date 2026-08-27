//! Contrats des plugins natifs partageant le même répertoire temporaire.

static PLUGIN_DATA_DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();

fn use_scratch_plugin_data_dir() {
    PLUGIN_DATA_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // Safety: this OnceLock performs the only write in this process, before
        // the caller constructs an AppState that can read the variable.
        unsafe {
            std::env::set_var("TUNE_PLUGINS_DATA_DIR", dir.path());
        }
        dir
    });
}

#[path = "dj_plugin.rs"]
mod dj_plugin;
#[path = "plugin_routes.rs"]
mod plugin_routes;
