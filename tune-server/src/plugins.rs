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

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use tune_core::plugin_sdk::{PluginLoader, PluginRegistrations, TunePlugin};

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
/// Empty in this tree, and it should stay that way for anything whose source
/// is not in this repository. A `path` dependency pointing outside the repo
/// breaks `cargo check` for every clone — cargo resolves optional path
/// dependencies while writing the lockfile, so `optional = true` does not save
/// you and neither does leaving the feature off. Such a plugin composes from
/// its own workspace instead, via [`PluginBuilder`] and
/// [`crate::bootstrap::run`]; this function is only for source that lives here.
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
async fn register_builtin_plugins(loader: &PluginLoader, state: &AppState) {
    // P5 (#917): DJ mode, extracted from the always-on core into a native
    // in-tree plugin. Host services are passed explicitly at construction so
    // DJ's real dependency (the DB backend) is visible here at the wiring site.
    #[cfg(feature = "dj")]
    loader
        .register(Box::new(tune_dj::DjPlugin::new(tune_dj::HostServices {
            backend: state.backend.clone(),
        })))
        .await;

    // Karaoke: native synced-lyrics plugin (model B). Reuses
    // `tune_core::lyrics` — no LRC parse/fetch reimplemented. Host services are
    // passed explicitly so its real dependencies (DB backend, HTTP client, and
    // the playback manager for `/now`) are visible here at the wiring site.
    #[cfg(feature = "karaoke")]
    loader
        .register(Box::new(tune_karaoke::KaraokePlugin::new(
            tune_karaoke::HostServices {
                backend: state.backend.clone(),
                client: state.http_client.clone(),
                playback: state.playback.clone(),
            },
        )))
        .await;

    // Bandcamp (#1768) : recherche, découverte et lecture, sorties du cœur
    // toujours-compilé. Le lot 1 n'avait besoin d'AUCUN service — il ne parlait
    // qu'HTTP à un tiers. Le lot 2 mémorise le pseudo et le `fan_id` résolu de
    // l'acheteur : il lui faut la base, passée explicitement ici comme pour les
    // deux autres plugins.
    #[cfg(feature = "bandcamp")]
    loader
        .register(Box::new(tune_bandcamp::BandcampPlugin::new(
            tune_bandcamp::HostServices {
                backend: state.backend.clone(),
            },
        )))
        .await;

    // Concerts (#2363) : la tâche d'abonnement 24 h et la route de lecture,
    // sorties du cœur toujours-compilé. Elle y était démarrée SANS condition
    // par `background.rs` — elle ne tourne désormais que chez ceux qui ont
    // installé le plugin. Sa seule dépendance est la base : lire les artistes
    // de la bibliothèque, et lire `instance_id` / `community_sync_enabled`.
    #[cfg(feature = "concerts")]
    loader
        .register(Box::new(tune_concerts::ConcertsPlugin::new(
            tune_concerts::HostServices {
                backend: state.backend.clone(),
            },
        )))
        .await;
}

/// Builds the plugins an out-of-tree binary wants registered.
///
/// Takes `&AppState` because a plugin's host services — `services`, `backend`,
/// `http_client` — only exist once state is built. Called once, from [`init`].
///
/// This is what makes a closed-source plugin possible: it composes from a
/// separate workspace that depends on `tune-server` as a library, so this
/// repository never names it and a plain clone still builds. See
/// [`crate::bootstrap::run`].
pub type PluginBuilder = Box<dyn FnOnce(&AppState) -> Vec<Box<dyn TunePlugin>> + Send>;

/// Set every plugin up, install what they registered, and start event
/// dispatch. Returns the routers for [`crate::routes::router`] to mount.
///
/// `extra` is registered alongside the compiled-in set, on equal terms: same
/// protocol gate, same `plugin_{name}_enabled` switch, same registration
/// draining. An out-of-tree plugin is not a second-class one.
pub async fn init(
    state: &AppState,
    api_base_url: &str,
    extra: Vec<Box<dyn TunePlugin>>,
) -> PluginRouters {
    let mut loader = state.plugins.lock().await;

    register_builtin_plugins(&loader, state).await;
    for plugin in extra {
        info!(plugin_name = %plugin.name(), "plugin_registered_out_of_tree");
        loader.register(plugin).await;
    }
    // Publish the registered set BEFORE `setup_all`, which retains only what
    // loaded — afterwards the loader no longer knows what this binary carries.
    // Published unconditionally, ahead of the early return: an empty list and
    // an unpublished one must mean the same thing to a reader, which they do
    // (`OnceLock::get() -> None` degrades to the empty slice).
    let _ = state.plugin_names.set(loader.registered_names().await);

    if loader.plugin_count().await == 0 {
        return Vec::new();
    }

    let loaded = loader.setup_all(api_base_url).await;

    // Publish the dormant set first, and unconditionally: opt-in plugins
    // (DJ/Karaoke) are the whole reason `loaded` can be empty while there is
    // still something for the plugin manager to list. Do it before the
    // early-return below or a fresh install — nothing loaded yet — would hide
    // them from the manager and leave the user no way to install them.
    let _ = state.plugin_available.set(loader.unloaded_plugins());

    if loaded.is_empty() {
        return Vec::new();
    }

    // Publish the introspection snapshot for the REST handlers. `setup_all`
    // already retained only what loaded, so this is a faithful list; the point
    // of copying it out is contention. Event dispatch holds the loader lock
    // across every plugin's `on_event`, so a handler taking the same lock would
    // hang for as long as the slowest plugin — while holding `state.plugins`,
    // which delays shutdown too. Nothing registers after init, so one snapshot
    // serves every request with no lock at all.
    let _ = state.plugin_info.set(loader.loaded_plugins().await);

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

    // device_ids this plugin actually got into the registry. A zone is only
    // created for one of these — see the zone loop below.
    let mut claimed: HashSet<String> = HashSet::new();

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
            claimed.insert(device_id);
        }
    }

    for zone in zones {
        // Only bind a zone to a device this plugin just claimed. Two reasons:
        // an id whose output was refused above belongs to *another* output, so
        // creating the zone anyway would quietly point the plugin's zone at
        // someone else's device; and an id no output ever claimed leaves an
        // orphan zone with nothing behind it.
        if !claimed.contains(&zone.device_id) {
            warn!(
                name = %zone.name,
                device_id = %zone.device_id,
                "plugin_zone_skipped — no output of this plugin claimed that device_id"
            );
            continue;
        }

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

// ---------------------------------------------------------------------------
// Wasm-plugin facade for the REST routes.
//
// `plugins_host` only exists under `plugins-wasm` (release builds all enable
// it); these always-compiled wrappers let `routes/{plugins,marketplace}.rs`
// call install/uninstall without sprinkling feature cfgs through handlers. In
// a build without the runtime, install honestly fails instead of pretending
// (the old stub set a settings flag and called that "installed").
// ---------------------------------------------------------------------------

/// Persist a downloaded marketplace archive on disk. Returns the manifest id.
#[cfg(feature = "plugins-wasm")]
pub(crate) fn persist_wasm_archive(data: &[u8]) -> Result<String, String> {
    crate::plugins_host::persist_plugin_archive(data)
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) fn persist_wasm_archive(_data: &[u8]) -> Result<String, String> {
    Err("plugin runtime not built (plugins-wasm feature disabled)".to_string())
}

/// Remove an installed wasm plugin's directory. `Ok(false)` when nothing was
/// on disk (including builds without the runtime).
#[cfg(feature = "plugins-wasm")]
pub(crate) fn remove_wasm_dir(id: &str) -> Result<bool, String> {
    crate::plugins_host::remove_plugin_dir(id)
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) fn remove_wasm_dir(_id: &str) -> Result<bool, String> {
    Ok(false)
}

/// The on-disk wasm plugins directory, when the runtime is built.
#[cfg(feature = "plugins-wasm")]
pub(crate) fn wasm_plugins_dir() -> Option<std::path::PathBuf> {
    Some(crate::plugins_host::plugins_dir())
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) fn wasm_plugins_dir() -> Option<std::path::PathBuf> {
    None
}

/// Probe-child dispatch for the wasm runtime (see
/// `plugins_host::maybe_run_wasm_probe`). Always-compiled facade so the
/// binary entry points can call it without feature cfgs; a build without
/// `plugins-wasm` has nothing to probe.
#[cfg(feature = "plugins-wasm")]
pub fn maybe_run_wasm_probe() {
    crate::plugins_host::maybe_run_wasm_probe();
}

#[cfg(not(feature = "plugins-wasm"))]
pub fn maybe_run_wasm_probe() {}
