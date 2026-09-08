pub(crate) mod helpers;
mod integration_test;
mod multiroom;
mod output;

pub use multiroom::{OaatMultiroomOutput, oaat_synchronization_contract};
pub use output::{OaatDiagnostics, OaatOutput};
