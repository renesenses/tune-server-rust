//! The output plugin contract lives in the `tune-output-api` crate so that
//! out-of-tree plugins (e.g. tune-diretta) can depend on it without pulling
//! in all of tune-core. Re-exported here so in-tree paths stay unchanged.

pub use tune_output_api::*;
