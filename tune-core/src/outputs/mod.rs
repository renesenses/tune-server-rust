pub mod airplay;
pub mod airplay2;
/// #4556 — l'état du coupe-circuit ASIO, et le refus qui sait le raconter.
///
/// Volontairement HORS de tout `cfg` : le refus est rendu par
/// `orchestrator::transport`, qui se compile aussi sans `local-audio`, et relu
/// par la route HTTP. Sans blocage posé, tout y rend `None` et rien ne change.
pub mod asio_blocage_4556;
#[cfg(all(target_os = "windows", feature = "asio"))]
pub mod asio_exclusive;
pub mod bluos;
pub mod bridge;
#[cfg(test)]
mod capabilities_test;
/// #5643 — les cadences DSD natives déclarées par chaque pilote ASIO, sondées
/// une fois. Hors `cfg` : l'API les lit partout, la règle se teste sous Linux.
pub mod capacite_dsd_natif;
pub mod chromecast;
#[cfg(all(target_os = "macos", feature = "local-audio"))]
pub mod coreaudio_exclusive;
pub mod didl;
pub mod dlna;
pub mod dlna_annonce_suivante;
pub mod dlna_buffer_stats;
pub(crate) mod dlna_contact;
pub(crate) mod dlna_journal_volume;
pub mod dlna_profil_volume;
/// Repli conservateur sur un refus de `SetAVTransportURI` (501/714/716),
/// mémorisé par appareil et conservé d'un démarrage à l'autre.
pub mod dlna_repli_set_uri;
#[cfg(test)]
mod dlna_repli_set_uri_tests;
#[cfg(test)]
mod dlna_test;
pub mod hqplayer;
pub mod identite_de_sortie;
#[cfg(feature = "local-audio")]
pub mod local;
/// #4357 — le masque de canaux (`dwChannelMask`) de l'ouverture WASAPI
/// exclusive. Hors FFI comme `negociation_format_exclusif_3837`, dont il
/// déroule la négociation pour chaque masque : jugé par `cargo test`.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod masque_de_canaux_4357;
pub mod mock;
/// #3837 — la négociation de format de la sortie WASAPI exclusive. Sans FFI
/// ni `cfg` de plateforme dans son corps : aucun job de CI n'exécute WASAPI,
/// cette logique-ci est donc jugée par `cargo test` sur Linux.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod negociation_format_exclusif_3837;
#[cfg(feature = "oaat")]
pub mod oaat;
pub mod oh_events;
pub mod openhome;
pub mod openhome_pins;
#[cfg(any(target_os = "windows", test))]
pub(crate) mod periode_exclusive_4357;
/// Les pseudo-périphériques ALSA (le PCM `null`, la carte `snd-dummy`). Hors
/// de `local-audio` à dessein : la porte `test` de la CI ne compile pas cette
/// feature pour `tune-core`, et cette décision-ci doit pouvoir y être jugée.
pub mod pseudo_peripherique_alsa;
pub mod registry;
/// #4357 — les réveils EN RETARD de cette même boucle, et la tâche MMCSS du
/// fil. Hors FFI pour la même raison : le seuil se juge par `cargo test`.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod reveil_en_retard_4357;
/// #4357 — le réveil de la boucle de rendu WASAPI exclusive. Même raison que
/// `negociation_format_exclusif_3837` d'être hors FFI : aucun job de CI
/// n'exécute WASAPI, cette table de décision-ci est jugée par `cargo test`.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod reveil_rendu_4357;
pub mod slimproto;
pub mod squeezebox;
/// #3967 — ce que le protocole permet de VÉRIFIER d'une suivante préparée,
/// éprouvé contre un vrai serveur SOAP.
#[cfg(test)]
mod suivante_verifiee_3967;
pub mod traits;
#[cfg(all(target_os = "windows", feature = "local-audio"))]
#[allow(unsafe_op_in_unsafe_fn)]
pub mod wasapi_exclusive;

pub use registry::OutputRegistry;
pub use traits::{
    OutputCapabilities, OutputCommand, OutputCommandError, OutputCommandResult, OutputStatus,
    OutputTarget, PlayMedia, SuivantePreparee, TransportState, VolumeResolution,
};
