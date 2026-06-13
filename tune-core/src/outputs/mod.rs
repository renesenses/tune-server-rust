pub mod airplay;
#[cfg(all(target_os = "windows", feature = "asio"))]
pub mod asio_exclusive;
pub mod bluos;
pub mod bridge;
pub mod chromecast;
#[cfg(all(target_os = "macos", feature = "local-audio"))]
pub mod coreaudio_exclusive;
pub mod didl;
pub mod dlna;
pub mod dlna_buffer_stats;
#[cfg(test)]
mod dlna_test;
pub mod hqplayer;
#[cfg(feature = "local-audio")]
pub mod local;
pub mod mock;
#[cfg(feature = "oaat")]
pub mod oaat;
pub mod oh_events;
pub mod openhome;
pub mod registry;
pub mod squeezebox;
pub mod traits;

pub use registry::OutputRegistry;
pub use traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};
