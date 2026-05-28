pub mod chromecast;
pub mod dlna;
#[cfg(feature = "local-audio")]
pub mod local;
pub mod registry;
pub mod traits;

pub use registry::OutputRegistry;
pub use traits::*;
