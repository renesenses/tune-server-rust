//! Experimental source-level SDK. No server dependency and no stable binary ABI.
//!
//! Control-plane operations may allocate. [`audio::Processor::process`] must not
//! allocate, block, perform I/O, query licensing or call host services. This is
//! an author obligation, not a guarantee enforced by the Rust type system.
//! The production host adapters and dynamic loader are not implemented here.
//!
//! ```
//! use tune_plugin_sdk::audio::{AudioBlock, AudioFormat, ChannelLayout, SampleEncoding, SamplesMut};
//! let format = AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::S32)?;
//! let mut pcm = [16_777_217, -16_777_217];
//! let block = AudioBlock::new(format, SamplesMut::S32(&mut pcm), 1)?;
//! assert_eq!(block.frames(), 1);
//! # Ok::<(), tune_plugin_sdk::Error>(())
//! ```
#![forbid(unsafe_code)]

pub mod audio;
pub mod batch;
pub mod manifest;
pub mod observation;
pub mod ui;

pub use serde_json::Value as Settings;

/// Allocation-free errors suitable for the audio thread. Human-readable
/// diagnostics can add context on the control thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidFormat,
    IncompleteFrame,
    BlockTooLarge,
    UnsupportedFormat,
    InvalidSettings,
    NonFinite,
    Cancelled,
    CapabilityMissing,
    InvalidState,
    InvalidObservation,
    HostFailure,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
