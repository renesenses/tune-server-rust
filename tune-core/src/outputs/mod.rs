pub mod airplay;
pub mod airplay2;
#[cfg(all(target_os = "windows", feature = "asio"))]
pub mod asio_exclusive;
pub mod bluos;
pub mod bridge;
#[cfg(test)]
mod capabilities_test;
pub mod chromecast;
#[cfg(all(target_os = "macos", feature = "local-audio"))]
pub mod coreaudio_exclusive;
pub mod didl;
pub mod dlna;
pub mod dlna_buffer_stats;
#[cfg(test)]
mod dlna_test;
pub mod hqplayer;
pub mod identite_de_sortie;
#[cfg(feature = "local-audio")]
pub mod local;
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
pub mod registry;
pub mod slimproto;
pub mod squeezebox;
pub mod traits;
#[cfg(all(target_os = "windows", feature = "local-audio"))]
#[allow(unsafe_op_in_unsafe_fn)]
pub mod wasapi_exclusive;

pub use registry::OutputRegistry;
pub use traits::{
    OutputCapabilities, OutputCommand, OutputCommandError, OutputCommandResult, OutputStatus,
    OutputTarget, PlayMedia, TransportState, VolumeResolution,
};
