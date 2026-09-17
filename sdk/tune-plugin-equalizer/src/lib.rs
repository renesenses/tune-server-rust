//! Premium equalizer; extracted without changing its arithmetic.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
mod engine;
pub use engine::*;
mod sdk;
pub use sdk::Equalizer;

#[cfg(feature = "native")]
tune_plugin_abi::export_dsp!(crate::Equalizer, include_str!("../manifest.json"));
