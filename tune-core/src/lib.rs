#![deny(unused_imports)]

pub mod error;
pub use error::TuneError;

pub mod ai;
pub mod alarms;
pub mod api_analytics;
pub mod audio;
pub mod bug_report;
pub mod cloud;
pub mod collaborative;
pub mod config;
pub mod credentials_vault;
pub mod dashboard;
pub mod db;
pub mod db_backup;
pub mod deezer_proxy;
pub mod digest;
pub mod discovery;
pub mod event_bus;
pub mod event_types;
pub mod health;
pub mod health_monitor;
pub mod http;
pub mod library;
pub mod license;
pub mod lyrics;
pub mod metadata;
pub mod mount_manager;
pub mod network;
pub mod notifications;
pub mod orchestrator;
pub mod outputs;
pub mod party_mode;
pub mod playback;
pub mod playback_history;
pub mod playlist_manager;
pub mod playlist_sync;
pub mod playlist_transfer;
pub mod plugin_sdk;
pub mod plugins;
pub mod poller;
pub mod prefetch;
pub mod queue_persistence;
pub mod radio_favorites;
pub mod radio_metadata;
pub mod remote_discovery;
pub mod remote_proxy;
pub mod scan_scheduler;
pub mod scanner;
pub mod scrobble;
pub mod services_manager;
pub mod sleep_timer;
pub mod slimproto;
pub mod smb_discovery;
pub mod stream_cache;
pub mod streaming;
pub mod updater;
pub mod upnp_server;
pub mod user_profiles;
pub mod zones;

pub fn version() -> &'static str {
    option_env!("TUNE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn rustc_version() -> &'static str {
    env!("TUNE_RUSTC_VERSION")
}

/// List of cargo features enabled at compile time.
pub fn enabled_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    #[cfg(feature = "local-audio")]
    features.push("local-audio");
    #[cfg(feature = "asio")]
    features.push("asio");
    #[cfg(feature = "oaat")]
    features.push("oaat");
    #[cfg(feature = "cloud-relay")]
    features.push("cloud-relay");
    #[cfg(feature = "postgres")]
    features.push("postgres");
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_semver() {
        let v = version();
        assert!(v.split('.').count() >= 3, "version must be semver: {v}");
    }
}
