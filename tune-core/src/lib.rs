pub mod alarms;
pub mod artist_enrichment;
pub mod artwork;
pub mod audio;
pub mod auto_fix;
pub mod batch_metadata;
pub mod bug_report;
pub mod buffer;
pub mod config;
pub mod credit_enricher;
pub mod cover_fetcher;
pub mod credentials_vault;
pub mod dashboard;
pub mod db;
pub mod db_backup;
pub mod deezer_proxy;
pub mod discovery;
pub mod duplicate_detector;
pub mod event_bus;
pub mod event_types;
pub mod export;
pub mod fingerprint;
pub mod full_text_search;
pub mod health;
pub mod health_monitor;
pub mod http;
pub mod lastfm_enrichment;
pub mod library_importer;
pub mod library_watcher;
pub mod m3u_parser;
pub mod metadata;
pub mod metadata_enrichment;
pub mod metadata_suggestions;
pub mod musicbrainz_release;
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
pub mod smart_collections;
pub mod stream_cache;
pub mod streaming;
pub mod tag_writer;
pub mod track_matcher;
pub mod genre_tree;
pub mod updater;
pub mod upnp_server;
pub mod user_profiles;
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
