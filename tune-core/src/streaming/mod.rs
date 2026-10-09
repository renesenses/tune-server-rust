pub mod amazon;
pub mod cadence_du_flux;
pub mod deezer;
pub mod deezer_decrypt;
pub mod favorites_date;
pub mod favorites_identity;
pub mod favorites_import;
pub mod matching;
pub mod podcasts;
pub mod qobuz;
pub mod qobuz_cles_brutes;
pub mod qobuz_credits;
pub mod quality;
pub mod radiofrance;
pub mod registry;
pub mod spotify;
pub mod spotify_connect;
/// #6018 — la lecture d'un titre Spotify par librespot, comme source PCM.
pub mod spotify_lecture;
pub mod tidal;
pub mod traits;
/// La vignette d'un podcast mise en cache à l'abonnement (#5214).
pub mod vignette_podcast;
pub mod youtube;

pub use registry::ServiceRegistry;
pub use traits::*;
