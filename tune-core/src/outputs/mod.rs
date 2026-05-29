pub mod airplay;
pub mod bluos;
pub mod chromecast;
pub mod dlna;
#[cfg(feature = "local-audio")]
pub mod local;
pub mod oh_events;
pub mod openhome;
pub mod registry;
pub mod traits;

pub use registry::OutputRegistry;
pub use traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};
