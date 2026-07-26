//! Plugin runtime wiring.
//!
//! `tune-core` owns the plugin *contract* ([`TunePlugin`], [`PluginLoader`]);
//! this module owns the *lifecycle*: which plugins exist, when they are set
//! up, and how what they register reaches the rest of the server.
//!
//! ## Loading model
//!
//! Plugins are compiled in, behind cargo features — there is no `libloading`
//! and no wasm runtime. `docs/ARCHITECTURE-CIBLE-v0.9.md` is explicit that
//! dynamic loading is a target, not the current state; adding it here would
//! mean pinning an ABI across rustc releases, which the roadmap defers.
//!
//! Adding a plugin is therefore three lines: a feature in `Cargo.toml`, an
//! optional dependency, and an arm in [`register_builtin_plugins`].
//!
//! ## Startup order
//!
//! [`init`] must run **after** `register_local_outputs` (so a plugin output
//! never races the local-device scan for the same zone row) and **before**
//! `routes::router` (which needs the plugin routers to mount them).

use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use tune_core::plugin_sdk::{PluginLoader, PluginRegistrations};

use crate::state::AppState;

/// Routers contributed by plugins, ready to be mounted by
/// [`crate::routes::router`]. `(plugin name, router)`.
pub type PluginRouters = Vec<(String, axum::Router<()>)>;

/// Build an empty, fully-configured loader.
///
/// Synchronous so `AppState::new` can own it. Plugins are attached later, in
/// [`init`], because `PluginLoader::register` is async.
pub fn build_loader(
    event_bus: &tune_core::event_bus::EventBus,
    backend: Arc<dyn tune_core::db::backend::DbBackend>,
) -> PluginLoader {
    PluginLoader::new(plugins_data_root())
        .with_event_bus(event_bus.clone())
        .with_db(backend)
}

/// Where plugins keep their private state. Each plugin gets
/// `{root}/{plugin_name}/`, created by `setup_all`.
fn plugins_data_root() -> std::path::PathBuf {
    std::env::var("TUNE_PLUGINS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("TUNE_PLUGINS_DIR")
                .map(|d| std::path::PathBuf::from(d).join("data"))
                .unwrap_or_else(|_| std::path::PathBuf::from("plugins/data"))
        })
}

/// The compiled-in plugin set.
///
/// Empty in this tree: no plugin ships in-repo yet. Adding one is three
/// edits, and an out-of-tree plugin must stay behind a non-default feature so
/// a plain clone still builds — cargo resolves optional path dependencies
/// when it writes the lockfile, so a path pointing outside the repo breaks
/// `cargo check` for everyone, feature enabled or not.
///
/// ```ignore
/// // 1. Cargo.toml — feature + optional dependency
/// //      myplugin = ["dep:tune-myplugin"]
/// //      tune-myplugin = { path = "../../plugins/tune-myplugin", optional = true }
/// // 2. here:
/// #[cfg(feature = "myplugin")]
/// loader
///     .register(Box::new(tune_myplugin::MyPlugin::new(
///         tune_myplugin::HostServices {
///             backend: state.backend.clone(),
///             http_client: state.http_client.clone(),
///         },
///     )))
///     .await;
/// ```
///
/// Host services are passed explicitly at construction rather than pulled
/// from [`PluginContext`], which deliberately exposes only the DB, the event
/// bus and a data directory — so a plugin's real dependencies are visible at
/// the wiring site.
#[allow(unused_variables)]
async fn register_builtin_plugins(loader: &PluginLoader, state: &AppState) {}

/// Set every plugin up, install what they registered, and start event
/// dispatch. Returns the routers for [`crate::routes::router`] to mount.
pub async fn init(state: &AppState, api_base_url: &str) -> PluginRouters {
    let mut loader = state.plugins.lock().await;

    register_builtin_plugins(&loader, state).await;
    if loader.plugin_count().await == 0 {
        return Vec::new();
    }

    let loaded = loader.setup_all(api_base_url).await;
    if loaded.is_empty() {
        return Vec::new();
    }

    let registrations = loader.take_registrations();
    let routers = install(state, registrations).await;

    loader.start_event_dispatch();
    info!(plugins = ?loaded, "plugins_ready");

    routers
}

/// Apply a drained [`PluginRegistrations`]: outputs into the registry, zones
/// into the DB, routers handed back to the caller.
async fn install(state: &AppState, registrations: PluginRegistrations) -> PluginRouters {
    // `routers` exists unconditionally here: tune-server always enables
    // tune-core's `plugin-http` feature (see its Cargo.toml).
    let PluginRegistrations {
        outputs,
        routers,
        zones,
    } = registrations;

    if !outputs.is_empty() {
        let mut registry = state.outputs.lock().await;
        for output in outputs {
            let device_id = output.device_id().to_string();
            // A plugin output claiming an id that discovery already owns would
            // silently replace a real device. Refuse rather than shadow it.
            if registry.contains(&device_id) {
                warn!(
                    device_id = %device_id,
                    "plugin_output_device_id_conflict — not registered"
                );
                continue;
            }
            info!(
                device_id = %device_id,
                name = %output.name(),
                output_type = %output.output_type(),
                "plugin_output_registered"
            );
            registry.register(output);
        }
    }

    for zone in zones {
        let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        match repo.get_or_create(&zone.name, Some(&zone.output_type), &zone.device_id) {
            Ok((zone_id, true)) => {
                info!(
                    zone_id,
                    name = %zone.name,
                    device_id = %zone.device_id,
                    "plugin_zone_created"
                );
            }
            Ok((_, false)) => {
                // Already there from a previous boot — just mark it reachable.
                let _ = repo.set_online_by_device(&zone.device_id, true);
            }
            Err(e) => {
                warn!(
                    name = %zone.name,
                    device_id = %zone.device_id,
                    error = %e,
                    "plugin_zone_create_failed"
                );
            }
        }
    }

    routers
}

/// Tear every plugin down. Called on graceful shutdown, before the process
/// exits, so a plugin can finish flushing whatever it holds open.
pub async fn shutdown(plugins: &Arc<Mutex<PluginLoader>>) {
    let mut loader = plugins.lock().await;
    if loader.plugin_count().await == 0 {
        return;
    }
    info!("plugins_shutting_down");
    loader.teardown_all().await;
}
