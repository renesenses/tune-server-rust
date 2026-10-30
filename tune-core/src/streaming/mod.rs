pub mod amazon;
/// Favoris radio → playlist chez un service (aperçu, puis création).
pub mod appariement_favoris_radio;
pub mod cadence_du_flux;
pub mod deezer;
pub mod deezer_decrypt;
pub mod favorites_date;
pub mod favorites_identity;
pub mod favorites_import;
pub mod favorites_mirror;
pub mod matching;
pub mod podcast_annuaires;
pub mod podcast_catalogue;
pub mod podcasts;
/// #6480 — le texte de présentation d'un album de service, HTML ramené à du texte.
pub mod presentation_album;
pub mod qobuz;
pub mod qobuz_cles_brutes;
pub mod qobuz_credits;
/// #6401 — repli en 16/44,1 d'un flux haute résolution qui échoue au démarrage.
pub mod qobuz_repli_qualite;
pub mod quality;
pub mod radiofrance;
pub mod registry;
pub mod spotify;
pub mod spotify_connect;
/// #6018 — la lecture d'un titre Spotify par librespot, comme source PCM.
pub mod spotify_lecture;
pub mod tidal;
pub mod titres_phares_ecoutes;
pub mod titres_phares_lastfm;
pub mod traits;
/// La vignette d'un podcast mise en cache à l'abonnement (#5214).
pub mod vignette_podcast;
pub mod youtube;
pub mod youtube_decouverte;

pub use registry::ServiceRegistry;
pub use traits::*;
