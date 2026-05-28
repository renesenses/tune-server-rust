pub mod deezer;
pub mod qobuz;
pub mod registry;
pub mod spotify;
pub mod tidal;
pub mod traits;
pub mod youtube;

pub use registry::ServiceRegistry;
pub use traits::*;
