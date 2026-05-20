pub mod traits;
pub mod registry;
pub mod dlna;
pub mod chromecast;
#[cfg(feature = "local-audio")]
pub mod local;

pub use traits::*;
pub use registry::OutputRegistry;
