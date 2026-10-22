//! Sortie réseau Devialet AIR, pour les Devialet Expert de première
//! génération (D200, D400…) qui n'acceptent que ce protocole (#6542).
//!
//! **Expérimental, désactivé par défaut.** Le protocole n'est ni documenté ni
//! approuvé par Devialet. Il est implémenté d'après la rétro-ingénierie
//! publiée par Rémi (fil 2336 du forum), reconstituée par capture passive du
//! trafic entre Roon et son ampli. Seuls des faits en sont repris : le code
//! et les vecteurs de test sont écrits ici. Personne chez nous n'a d'Expert :
//! rien n'a été essayé sur un vrai ampli avant la rc7.
//!
//! Tout passe en UDP, sans chiffrement ni authentification :
//!
//! - [`protocole`] : les quatre messages (`Da` annonce, `Dr` commande,
//!   `Dm` audio, `Df` retour), le codec et le CRC — pur, sans réseau ;
//! - [`ecoute`] : l'écoute passive des annonces (port 45454) ;
//! - [`emetteur`] : un flux audio, sa cadence et ses retransmissions ;
//! - [`sortie`] : l'`OutputTarget` d'un ampli ;
//! - [`fournisseur`] : l'`OutputProvider`, une sortie par ampli entendu ;
//! - [`reglages`] : l'option, l'indice de source, les politiques de cadence
//!   et de volume.
//!
//! Garde-fous de cette version : plafond de volume à −10 dB ; 96 kHz au plus,
//! sans perte (rééchantillonnage au-delà) ; stéréo ; pas d'enchaînement sans
//! coupure ; aucune lutte avec un autre hôte (Roon) qui sert l'ampli.

pub mod ecoute;
pub mod emetteur;
pub mod fournisseur;
pub mod protocole;
pub mod reglages;
pub mod sortie;

#[cfg(test)]
mod simulateur;
#[cfg(test)]
mod tests;

pub use fournisseur::FournisseurDevialetAir;
pub use sortie::{ContexteAir, DevialetAirOutput, TYPE_DE_SORTIE};
