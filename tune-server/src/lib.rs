#![recursion_limit = "256"]

pub mod auth;
pub mod auto_resume;
pub mod auto_scan;
pub mod background;
pub mod background_tasks;
pub mod bootstrap;
pub mod config;
pub mod discovery_setup;
pub mod error;
pub mod i18n;
pub mod plugins;
/// P2 of the plugin ABI: AppState-backed [`HostContext`] plus the registry of
/// loaded wasm plugins. Gated behind `plugins-wasm`; absent from default builds.
#[cfg(feature = "plugins-wasm")]
pub mod plugins_host;
pub mod premium_guard;
pub mod routes;
pub mod run;
pub mod scan_import;
pub mod startup;
pub mod state;
pub mod windows_migrate;

/// The whole server startup, so out-of-tree binaries can compose it with their
/// own plugins. See [`bootstrap::run`].
pub use bootstrap::run;
