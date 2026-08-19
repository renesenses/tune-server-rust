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
pub mod config_backup;
pub mod credentials_vault;
pub mod dac_calibration;
pub mod dashboard;
pub mod db;
pub mod db_backup;
pub mod deezer_proxy;
pub mod device_catalog;
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
/// P0 of the plugin ABI (RFC §3): embedded wasmtime runtime that loads,
/// instantiates and calls wasm plugins with JSON-over-linear-memory
/// marshalling under resource limits. Gated behind `plugins-wasm`.
#[cfg(feature = "plugins-wasm")]
pub mod plugins_runtime;
pub mod poller;
pub mod prefetch;
pub mod queue_persistence;
pub mod radio_favorites;
pub mod radio_metadata;
pub mod remote_discovery;
pub mod remote_proxy;
pub mod room_correction;
pub mod scanner;
pub mod scrobble;
pub mod secret_envelope;
pub mod services_manager;
pub mod skins;
pub mod sleep_timer;
pub mod slimproto;
pub mod smb_discovery;
pub mod social;
pub mod stream_cache;
pub mod streaming;
pub mod transcode_cache;
pub mod updater;
pub mod upnp_renderer;
pub mod upnp_server;
pub mod user_profiles;
pub mod ytdlp;
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

#[cfg(test)]
mod manifeste_tests {
    /// Une dependance ajoutee a la suite de `libc` herite silencieusement de
    /// `[target.'cfg(unix)'.dependencies]`.
    ///
    /// C'est ce qui est arrive aux cinq bibliotheques de l'appairage AirPlay 2
    /// (#1911) : sur Windows elles n'existaient pas, alors que
    /// `outputs/airplay2/pairing.rs` n'est restreint a rien. La ligne de
    /// release ne compilait plus sous Windows, et seulement sous Windows —
    /// Linux et macOS passaient, donc le defaut n'apparaissait qu'au moment de
    /// construire les artefacts.
    ///
    /// Ce test lit le manifeste : la section `cfg(unix)` ne doit contenir que
    /// `libc`. Toute autre dependance qui s'y retrouve est presque surement
    /// tombee la par accident.
    #[test]
    fn la_section_unix_ne_contient_que_libc() {
        let manifeste = include_str!("../Cargo.toml");

        // Par LIGNES, et non par decoupage sur la chaine : cet en-tete
        // apparait aussi dans le commentaire qui explique le piege, et un
        // `split` naif tomberait dessus. Le piege attrape jusqu'a son propre
        // garde-fou.
        let mut dedans = false;
        let mut dependances: Vec<&str> = Vec::new();
        for ligne in manifeste.lines() {
            let l = ligne.trim();
            if l.starts_with('[') {
                dedans = l == "[target.'cfg(unix)'.dependencies]";
                continue;
            }
            if dedans && !l.is_empty() && !l.starts_with('#') {
                if let Some(nom) = l.split('=').next() {
                    dependances.push(nom.trim());
                }
            }
        }

        assert_eq!(
            dependances,
            vec!["libc"],
            "la section cfg(unix) ne doit contenir que `libc`. Les autres y \
             tombent par accident — on ajoute a la suite sans voir l'en-tete — \
             et disparaissent de la compilation Windows sans que rien ne le \
             signale avant la construction des artefacts."
        );
    }
}
