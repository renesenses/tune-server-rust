//! Étape E2 : le suiveur PTPv2 (IEEE 1588-2008, profil par défaut d'AES67)
//! derrière [`crate::horloge::SourceHorloge`].
//!
//! - [`horloge_virtuelle`] : l'horloge que `statime` asservit, posée sur
//!   l'horloge monotone, sans aucun droit sur l'horloge du système ;
//! - [`transport`] : UDP 319/320, multicast 224.0.1.129, horodatage
//!   LOGICIEL ;
//! - [`suiveur`] : l'instance `statime` esclave seulement, le calage, le
//!   repli explicite sur l'horloge locale ;
//! - [`banc`] : un maître PTP simulé, indépendant de `statime`, pour les
//!   tests et le banc (jamais en production).

pub mod banc;
pub mod horloge_virtuelle;
pub mod suiveur;
pub mod transport;

pub use suiveur::{ConfigPtp, EtatPtp, SuiveurPtp};
