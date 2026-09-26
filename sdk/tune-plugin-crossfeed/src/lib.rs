//! Premium crossfeed; historical Mid-preserving algorithm.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
mod engine;
pub use engine::*;
mod ombre;
pub use ombre::{
    COUPURE_DEFAUT_HZ, COUPURE_MAX_HZ, COUPURE_MIN_HZ, OmbreDeTete, PENTE_DEFAUT_DB_OCT,
    PENTE_MAX_DB_OCT, PENTE_MIN_DB_OCT,
};
mod sdk;
pub use sdk::{Crossfeed, CrossfeedSettings};

#[cfg(feature = "native")]
tune_plugin_abi::export_dsp!(crate::Crossfeed, include_str!("../manifest.json"));
