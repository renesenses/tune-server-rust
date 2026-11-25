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

/// TESS T05 — capability a DSP manifest requests to receive `F64` blocks.
///
/// ABI 1 already carries `F64` buffers (encoding 5); this capability is the
/// negotiation. A plugin that requests it MUST accept `F64` in `prepare` and
/// `process`; the host refuses to load one that declares it and refuses `F64`.
/// A plugin that does not request it is never given `F64`: the host converts
/// at its boundary (`f64` -> `f32` -> plugin -> `f64`), so every existing
/// `F32` package keeps running unchanged. Request it `required: false` to stay
/// loadable, in `F32`, by a host that predates it; `required: true` makes such
/// a host refuse the package instead.
pub const AUDIO_F64: &str = "audio-f64";

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

/// TESS T05 — membership of a mutually exclusive group of DSP stages: a chain
/// never enables two members of one group (for example `crossfeed_crosstalk`).
/// A larger `priority` wins when an old chain still enables several.
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusiveGroup {
    pub id: String,
    pub priority: u8,
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
    /// TESS T05 — optional exclusive group. Absent in every package published
    /// before T05; the host still recognises the `crossfeed-` prefix for them.
    /// Its id follows the identifier rules with `_` allowed
    /// (`crossfeed_crosstalk`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_group: Option<ExclusiveGroup>,
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
    InvalidExclusiveGroup,
}

/// Group identifiers: lowercase ASCII, digits, `-` and `_`, 1 to 64 bytes,
/// starting with a letter.
pub fn valid_group_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
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
        if let Some(group) = &self.exclusive_group
            && (!valid_group_id(&group.id) || group.priority == 0)
        {
            return Err(ManifestError::InvalidExclusiveGroup);
        }
        Ok(())
    }

    /// TESS T05 — this DSP plugin asks to receive `F64` blocks
    /// ([`AUDIO_F64`]), whether the request is required or optional. Only a
    /// host that offers the capability acts on it.
    pub fn declares_f64(&self) -> bool {
        self.kind == PluginKind::Dsp && self.capabilities.iter().any(|c| c.id == AUDIO_F64)
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
        AUDIO_F64,
    ]
    .into_iter()
    .map(|id| (id.to_string(), SDK_VERSION))
    .collect()
}
