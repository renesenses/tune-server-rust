pub mod artwork;
pub mod audio;
pub mod buffer;
pub mod db;
pub mod discovery;
pub mod event_bus;
pub mod http;
pub mod metadata;
pub mod orchestrator;
pub mod outputs;
pub mod playback;
pub mod poller;
pub mod radio_metadata;
pub mod scanner;
pub mod scrobble;
pub mod streaming;
pub mod upnp_server;

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
