#![deny(unused_imports)]

pub mod error;
pub use error::TuneError;

pub mod ai;
pub mod alarms;
pub mod api_analytics;
pub mod audio;
pub mod bandcamp_veille;
pub mod bug_report;
pub mod cadence;
pub mod chemins_de_travail;
pub mod cloud;
pub mod collaborative;
pub mod confidentialite;
pub mod config;
pub mod config_backup;
pub mod config_export;
pub mod credentials_vault;
pub mod dac_calibration;
pub mod dashboard;
pub mod db;
pub mod db_backup;
pub mod deezer_proxy;
pub mod device_catalog;
pub mod device_preconfig;
pub mod digest;
pub mod discovery;
pub mod event_bus;
pub mod event_types;
pub mod favorites_sort;
pub mod health;
pub mod health_monitor;
pub mod http;
pub mod interface_web;
/// Capture de journal `tracing` fiable dans les tests de la lib — voir le
/// module pour le pourquoi (#5440 : un point d'appel figé à `never` par un
/// test voisin qui l'atteint le premier sur un autre fil).
///
/// #5453 : les tests de `tune-server` en ont besoin aussi. La feature
/// `journal-de-test` n'est allumée que par ses `[dev-dependencies]` : le
/// module n'entre dans aucun binaire publié.
#[cfg(any(test, feature = "journal-de-test"))]
#[doc(hidden)]
pub mod journal_de_test;
pub mod library;
pub mod license;
pub mod lyrics;
pub mod memoire_rendue;
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
pub mod secrets;
pub mod sendspin;
pub mod services_manager;
pub mod skins;
pub mod sleep_timer;
pub mod slimproto;
pub mod smb_discovery;
pub mod social;
pub mod source_pcm;
pub mod source_url;
pub mod sources_physiques;
pub mod stream_cache;
pub mod streaming;
mod system_sleep;
/// Pause des traitements de fond — voir le module pour le pourquoi : les
/// passes durent des heures et seul le scan avait un geste pour les arrêter.
pub mod taches_de_fond;
/// Temporisation des boucles d'ecoute reseau apres une erreur — voir le module
/// pour le pourquoi (issue #2156 : une erreur persistante sur `accept()`
/// saturait un coeur et remplissait le disque au repos).
pub mod temporisation_reseau;
/// Chemins temporaires uniques par appel — voir le module pour le pourquoi
/// (issue #2864 : deux tests du même binaire se volaient leur fichier).
///
/// `pub` et non `#[cfg(test)]` : `cfg(test)` ne traverse PAS les frontières
/// de caisse. Les binaires de test de `tune-server` — y compris les cibles
/// agrégées `server_contracts` et `appliance_contracts` — en ont besoin
/// aussi, et une seconde copie du compteur ne protégerait plus rien.
#[doc(hidden)]
pub mod test_scratch;
pub mod transcode_cache;
pub mod updater;
pub mod upnp_renderer;
pub mod upnp_server;
pub mod user_profiles;
pub mod ytdlp;
pub mod zones;

/// Version d'une construction faite SANS `TUNE_VERSION` : construction locale,
/// `docker build` du `Dockerfile` de développement, `cargo install` depuis les
/// sources.
///
/// Le suffixe `-dev` n'est pas cosmétique. Les fichiers gardent la base
/// `X.Y.Z` (convention A, `scripts/bump-all.sh`) et le suffixe de préversion
/// vit sur le tag seul. Sans `TUNE_VERSION`, un binaire construit pendant le
/// train `1.0.0-rcN` se disait donc `1.0.0` — et, en semver, `1.0.0` passe
/// DEVANT `1.0.0-rc3` : la vérification de mise à jour, côté serveur
/// ([`updater::select_release`]) comme côté client web, jugeait l'installation
/// plus récente que toutes les RC et ne lui en proposait jamais aucune, ni même
/// la `1.0.0` finale.
///
/// `X.Y.Z-dev` reste au-dessous de toute `X.Y.Z-rcN` et de la `X.Y.Z` finale,
/// et garde la même base `X.Y.Z` que le client web embarqué : l'écran « À
/// propos » ne crie pas à la dérive.
const VERSION_SANS_TAG: &str = concat!(env!("CARGO_PKG_VERSION"), "-dev");

/// `TUNE_VERSION` si la construction l'a reçu, non vide ; le repli sinon.
///
/// Le cas vide compte : un `ARG TUNE_VERSION` de Dockerfile sans valeur pose
/// une variable VIDE, et `option_env!` rend alors `Some("")`.
const fn choisir_version(tune_version: Option<&'static str>, repli: &'static str) -> &'static str {
    match tune_version {
        Some(v) if !v.is_empty() => v,
        _ => repli,
    }
}

/// La version du binaire. `release.yml` et `docker.yml` posent `TUNE_VERSION`
/// depuis le tag (`1.0.0-rc2`) ; toute autre construction rend
/// [`VERSION_SANS_TAG`].
pub fn version() -> &'static str {
    choisir_version(option_env!("TUNE_VERSION"), VERSION_SANS_TAG)
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use crate::updater::is_newer;

    const BASE: &str = env!("CARGO_PKG_VERSION");

    #[test]
    fn une_construction_sans_tag_ne_passe_devant_aucune_rc_de_sa_base() {
        for n in 1..=12 {
            let rc = format!("{BASE}-rc{n}");
            assert!(
                is_newer(&rc, VERSION_SANS_TAG),
                "{rc} doit être proposée à une construction sans tag ({VERSION_SANS_TAG})"
            );
            assert!(
                !is_newer(VERSION_SANS_TAG, &rc),
                "{VERSION_SANS_TAG} ne doit pas se croire plus récente que {rc}"
            );
        }
        assert!(
            is_newer(BASE, VERSION_SANS_TAG),
            "la {BASE} finale doit être proposée à {VERSION_SANS_TAG}"
        );
    }

    #[test]
    fn la_version_sans_tag_garde_la_base_du_workspace() {
        assert_eq!(VERSION_SANS_TAG.split('-').next(), Some(BASE));
        assert!(
            VERSION_SANS_TAG.contains('-'),
            "sans suffixe, une construction locale se dirait la finale"
        );
    }

    #[test]
    fn tune_version_prime_et_le_vide_retombe_sur_le_repli() {
        assert_eq!(choisir_version(Some("1.0.0-rc3"), "x-dev"), "1.0.0-rc3");
        assert_eq!(choisir_version(Some(""), "x-dev"), "x-dev");
        assert_eq!(choisir_version(None, "x-dev"), "x-dev");
        if option_env!("TUNE_VERSION").is_none_or(str::is_empty) {
            assert_eq!(version(), VERSION_SANS_TAG);
        }
    }
}

pub fn rustc_version() -> &'static str {
    env!("TUNE_RUSTC_VERSION")
}

/// List of cargo features enabled at compile time.
// Chaque `push` dépend d'un `cfg` : un `vec![]` littéral ne sait pas les
// porter (clippy 1.98, `vec_init_then_push`).
#[allow(clippy::vec_init_then_push)]
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
