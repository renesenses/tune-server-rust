//! Versioned messages for a future UI bridge. These types do not mount a
//! panel. The host must authenticate the sender, scope grants and enforce the
//! negotiated permissions before dispatch. Never expose internal Svelte stores.
use crate::{
    manifest::{SDK_VERSION, Version},
    observation::ObservationPoint,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiContext {
    pub protocol: Version,
    pub plugin_id: String,
    pub zone_id: Option<i64>,
    pub locale: String,
    pub theme: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCommand {
    GetConfiguration,
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
