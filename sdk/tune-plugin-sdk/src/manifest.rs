//! Negotiation runs before setup; required and optional capabilities differ.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}
pub const SDK_VERSION: Version = Version { major: 0, minor: 1 };

impl Version {
    pub fn supports(self, required: Self) -> bool {
        self.major == required.major
            && if self.major == 0 {
                self.minor == required.minor
            } else {
                self.minor >= required.minor
            }
    }
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Dsp,
    Batch,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequest {
    pub id: String,
    pub version: Version,
    pub required: bool,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub sdk: Version,
    pub kind: PluginKind,
    pub config_version: u32,
    pub capabilities: Vec<CapabilityRequest>,
    /// Entitlement identifier; the host maps it to existing subscription tiers.
    pub entitlement: String,
    /// Implementation composition; native packages wrap this source manifest
    /// with a separately versioned ABI and target envelope.
    pub distribution: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    InvalidId,
    UnsupportedSdk,
    InvalidConfigVersion,
    InvalidEntitlement,
    UnsupportedDistribution,
    DuplicateCapability(String),
    MissingCapability(String),
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !valid_id(&self.id) {
            return Err(ManifestError::InvalidId);
        }
        if !SDK_VERSION.supports(self.sdk) {
            return Err(ManifestError::UnsupportedSdk);
        }
        if self.config_version == 0 {
            return Err(ManifestError::InvalidConfigVersion);
        }
        if self.entitlement.trim().is_empty() {
            return Err(ManifestError::InvalidEntitlement);
        }
        if self.distribution != "source" {
            return Err(ManifestError::UnsupportedDistribution);
        }
        let mut seen = BTreeSet::new();
        for c in &self.capabilities {
            if !valid_id(&c.id) {
                return Err(ManifestError::InvalidId);
            }
            if !seen.insert(&c.id) {
                return Err(ManifestError::DuplicateCapability(c.id.clone()));
            }
        }
        Ok(())
    }

    /// Returns only capabilities both requested and supported. A host's other
    /// capabilities must never accidentally become permissions for this plugin.
    pub fn negotiate(
        &self,
        host: &BTreeMap<String, Version>,
    ) -> Result<BTreeSet<String>, ManifestError> {
        self.validate()?;
        let mut granted = BTreeSet::new();
        for c in &self.capabilities {
            if host.get(&c.id).is_some_and(|v| v.supports(c.version)) {
                granted.insert(c.id.clone());
            } else if c.required {
                return Err(ManifestError::MissingCapability(c.id.clone()));
            }
        }
        Ok(granted)
    }
}

/// Versioned services implemented by Tune's premium audio host adapters.
/// This inventory is shared by author checks, signed-package validation and
/// startup negotiation. A new service also needs an adapter and matrix witness.
pub fn reference_host_capabilities() -> BTreeMap<String, Version> {
    [
        "audio-process",
        "audio-live-update",
        "zone-settings",
        "audio-observation",
        "file-jobs",
        "audio-codecs",
        "metadata-copy",
        "atomic-artifacts",
        "cancellation",
    ]
    .into_iter()
    .map(|id| (id.to_string(), SDK_VERSION))
    .collect()
}
