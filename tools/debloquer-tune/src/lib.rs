//! Débloquer Tune — mode d'emploi : `NOTES-RELEASE.md`.
//!
//! Un serveur Tune OS en rc12 refuse sa mise à jour tant que l'authentification
//! est coupée (le port 80 y est relayé depuis 127.0.0.1), et l'interface de la
//! rc12 n'a aucun bouton pour l'activer. Cet outil le fait depuis une page
//! locale : choisir un mot de passe, activer l'authentification, lancer la mise
//! à jour, suivre le redémarrage. Il ne touche à aucune version rc13 ou plus.

pub mod adresse;
pub mod decouverte;
pub mod tune;
pub mod version;
pub mod web;
