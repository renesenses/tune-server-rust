//! TESS T13 — real-time click removal (`declick-live`, Premium).
//!
//! Detection by the matched-filtered prediction error of an AR model,
//! least-squares AR (LSAR) interpolation, bounded lookahead announced to the
//! host as latency. Internal computation in `f64`. Method note:
//! `docs/tess/declick-live.md`. Distinct from the batch `declick` tool.
#![forbid(unsafe_code)]
pub mod ar;
pub mod corpus;
pub mod engine;
pub mod sdk;

pub use engine::{Config, DeclickLive, Event, Stats};
pub use sdk::{DeclickLiveFactory, DeclickLiveSettings, MANIFEST};
