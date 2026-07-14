pub mod amazon;
pub mod deezer;
pub mod deezer_decrypt;
pub mod podcasts;
pub mod qobuz;
pub mod radiofrance;
pub mod registry;
pub mod spotify;
pub mod spotify_connect;
pub mod tidal;
pub mod traits;
pub mod youtube;

pub use registry::ServiceRegistry;
pub use traits::*;
