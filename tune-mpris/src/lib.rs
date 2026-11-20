//! Compagnon MPRIS de Tune pour Linux (#8824).
//!
//! Publie une zone Tune sur le bus D-Bus de la session comme lecteur MPRIS
//! (`org.mpris.MediaPlayer2.Tune`) : les touches multimédia, les applets de
//! GNOME et KDE et `playerctl` la pilotent et affichent ce qu'elle joue.
//! Le serveur peut tourner ailleurs (NAS, Pi, Docker) : le compagnon lui
//! parle par l'API HTTP et le WebSocket, comme le mini-lecteur.
//!
//! Seul [`bus`] dépend de D-Bus, et seulement sous Linux ; le reste se teste
//! partout.

#[cfg(target_os = "linux")]
pub mod bus;
pub mod client;
pub mod etat;
pub mod pochette;
pub mod requetes;
pub mod synchro;
pub mod volume;
