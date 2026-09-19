//! Premium crossfeed; historical Mid-preserving algorithm.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
mod engine;
pub use engine::*;
mod sdk;
pub use sdk::{Crossfeed, CrossfeedSettings};

#[cfg(feature = "native")]
tune_plugin_abi::export_dsp!(crate::Crossfeed, include_str!("../manifest.json"));
