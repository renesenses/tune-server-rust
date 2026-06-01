#![deny(unused_imports)]

pub mod alarms;
pub mod api_analytics;
pub mod audio;
pub mod buffer;
pub mod bug_report;
pub mod config;
pub mod credentials_vault;
pub mod dashboard;
pub mod db;
pub mod db_backup;
pub mod deezer_proxy;
pub mod discovery;
pub mod event_bus;
pub mod event_types;
pub mod health;
pub mod health_monitor;
pub mod http;
pub mod library;
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
pub mod radio_favorites;
pub mod radio_metadata;
pub mod remote_discovery;
pub mod remote_proxy;
pub mod scan_scheduler;
pub mod scanner;
pub mod scrobble;
pub mod services_manager;
pub mod sleep_timer;
pub mod smb_discovery;
pub mod stream_cache;
pub mod streaming;
pub mod updater;
pub mod upnp_server;
pub mod user_profiles;
pub mod zones;

// Re-exports for backward compatibility (modules moved into library/)
pub use library::artwork;
pub use library::cover_fetcher;
pub use library::duplicate_detector;
pub use library::export;
pub use library::full_text_search;
pub use library::genre_tree;
pub use library::importer as library_importer;
pub use library::m3u_parser;
pub use library::smart_collections;
pub use library::track_matcher;
pub use library::watcher as library_watcher;

// Re-exports for backward compatibility (modules moved into metadata/)
pub use metadata::artist_enrichment;
pub use metadata::auto_fix;
pub use metadata::batch as batch_metadata;
pub use metadata::credit_enricher;
pub use metadata::enrichment as metadata_enrichment;
pub use metadata::fingerprint;
pub use metadata::lastfm as lastfm_enrichment;
pub use metadata::matcher as metadata_matcher;
pub use metadata::musicbrainz_release;
pub use metadata::suggestions as metadata_suggestions;
pub use metadata::tag_writer;

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
