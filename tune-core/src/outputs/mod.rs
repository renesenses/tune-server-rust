pub mod airplay;
pub mod mock;
pub mod bluos;
pub mod bridge;
pub mod chromecast;
pub mod dlna;
pub mod dlna_buffer_stats;
#[cfg(feature = "local-audio")]
pub mod local;
pub mod oaat;
pub mod oh_events;
pub mod openhome;
pub mod registry;
pub mod squeezebox;
pub mod traits;

pub use registry::OutputRegistry;
pub use traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};
