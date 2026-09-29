//! Prélecture des crédits hors transaction et arrêt entre deux fichiers (#5202).
//!
//! Le mécanisme vit dans [`crate::lecture_bornee`], partagé avec le scan
//! automatique et les pochettes de l'importeur.
use std::time::Duration;

#[cfg(test)]
pub(super) use crate::lecture_bornee::EXPIRATIONS_AVANT_ABANDON;
pub(super) use crate::lecture_bornee::{LecteurMetadonnees, lire_metadonnees_du_lot};

/// Délai accordé à la relecture des crédits d'UN fichier (#5202).
pub(super) const DELAI_LECTURE_CREDITS: Duration = crate::lecture_bornee::DELAI_LECTURE_DISQUE;
