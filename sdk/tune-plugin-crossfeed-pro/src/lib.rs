//! Crossfeed Pro (#5039, phase 1) : crossfeed casque premium, Mid conservé,
//! voie croisée filtrée (ombre de la tête, coupe-bas), garde de phase et
//! préréglages libbs2b. Le greffon `crossfeed` (v1) reste inchangé à côté.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
mod engine;
pub use engine::*;
mod sdk;
pub use sdk::{CrossfeedPro, CrossfeedProSettings, presets};

#[cfg(feature = "native")]
tune_plugin_abi::export_dsp!(crate::CrossfeedPro, include_str!("../manifest.json"));
