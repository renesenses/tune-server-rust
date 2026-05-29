pub mod alarms;
pub mod artwork;
pub mod audio;
pub mod auto_fix;
pub mod buffer;
pub mod cover_fetcher;
pub mod credentials_vault;
pub mod db;
pub mod discovery;
pub mod duplicate_detector;
pub mod event_bus;
pub mod fingerprint;
pub mod health;
pub mod http;
pub mod library_watcher;
pub mod metadata;
pub mod metadata_enrichment;
pub mod network;
pub mod notifications;
pub mod orchestrator;
pub mod outputs;
pub mod playback;
pub mod playlist_manager;
pub mod playlist_sync;
pub mod playlist_transfer;
pub mod plugins;
pub mod poller;
pub mod radio_metadata;
pub mod remote_discovery;
pub mod remote_proxy;
pub mod scan_scheduler;
pub mod scanner;
pub mod scrobble;
pub mod sleep_timer;
pub mod smart_collections;
pub mod streaming;
pub mod tag_writer;
pub mod track_matcher;
pub mod genre_tree;
pub mod updater;
pub mod upnp_server;
pub mod zones;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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
