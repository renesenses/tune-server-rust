//! #8671 — mêmes modules et mêmes témoins que le cœur, exécutables sur Windows
//! sans compiler tout son banc et sans ouvrir de périphérique audio.
//! `rustc --edition 2024 --test scripts/banc-cadences-windows-8671.rs -o <banc>`

#[path = "../tune-core/src/outputs/cadence_exclusive_8616.rs"]
mod cadence_exclusive_8616;
#[path = "../tune-core/src/outputs/masque_de_canaux_4357.rs"]
mod masque_de_canaux_4357;
#[path = "../tune-core/src/outputs/negociation_format_exclusif_3837.rs"]
pub(crate) mod negociation_format_exclusif_3837;

// Un témoin des masques utilise le chemin absolu du module dans tune-core.
mod outputs {
    pub(crate) use crate::negociation_format_exclusif_3837;
}
