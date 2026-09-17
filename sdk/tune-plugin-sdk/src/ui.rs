//! Versioned messages for a future UI bridge. These types do not mount a
//! panel. The host must authenticate the sender, scope grants and enforce the
//! negotiated permissions before dispatch. Never expose internal Svelte stores.
use crate::{
    manifest::{SDK_VERSION, Version},
    observation::ObservationPoint,
};
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiContext {
    pub protocol: Version,
    pub plugin_id: String,
    pub zone_id: Option<i64>,
    pub locale: String,
    pub theme: String,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCommand {
    GetConfiguration,
    SetConfiguration {
        configuration: crate::Settings,
    },
    GetContext,
    CodecCapabilities,
    StartJob {
        options: crate::Settings,
    },
    EqualizerOperation {
        path: String,
        method: String,
        body: Option<crate::Settings>,
    },
    FrequencyResponse {
        sample_rate: u32,
        channels: u16,
    },
    SubscribeLevels {
        zone_id: i64,
        point: ObservationPoint,
    },
    Unsubscribe {
        subscription_id: String,
    },
    JobStatus {
        job_id: String,
    },
    CancelJob {
        job_id: String,
    },
    Download {
        artifact_id: String,
    },
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiRequest {
    pub protocol: Version,
    pub request_id: u64,
    pub plugin_id: String,
    pub action: UiCommand,
}

impl UiRequest {
    pub fn matches_session(&self, context: &UiContext) -> bool {
        SDK_VERSION.supports(self.protocol)
            && context.protocol == self.protocol
            && self.plugin_id == context.plugin_id
    }
}

/// Additive wire shape of the existing playback.audio_levels event. Arrays
/// share a frequency axis. Resolution refers to actual signal frames. Clients
/// must also validate finite numbers, equal lengths and zone/epoch freshness.
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioLevelsEvent {
    pub zone_id: i64,
    pub play_seq: u64,
    pub generation: u64,
    pub position_ms: f64,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
    pub observation_point: ObservationPoint,
    pub provenance: crate::observation::Provenance,
    pub spectrum: Vec<f64>,
    pub spectrum_db: Vec<f64>,
    pub spectrum_hz: Vec<f64>,
    pub spectrum_resolved: Vec<bool>,
    pub spectrum_resolution_hz: f64,
}
