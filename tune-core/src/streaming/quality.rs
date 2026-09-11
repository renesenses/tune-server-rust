//! Per-zone streaming quality contract.
//!
//! The web client exposes four product-level preferences. Streaming providers
//! use different names for them, so the orchestrator translates the stable
//! preference at the last possible moment instead of persisting provider API
//! constants in zone settings.

use serde::{Deserialize, Serialize};

use super::traits::StreamUrl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingQualityObservation {
    pub requested: &'static str,
    pub provider_token: &'static str,
    pub delivered_codec: String,
    pub delivered_sample_rate: u32,
    pub delivered_bit_depth: u16,
    pub delivered_bitrate: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamingQualityPreference {
    #[default]
    Max,
    Hires,
    Cd,
    Low,
}

impl StreamingQualityPreference {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Hires => "hires",
            Self::Cd => "cd",
            Self::Low => "low",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "max" => Some(Self::Max),
            "hires" => Some(Self::Hires),
            "cd" => Some(Self::Cd),
            "low" => Some(Self::Low),
            _ => None,
        }
    }

    /// Decode the setting stored by both the fixed endpoint (`{"quality":
    /// "cd"}`) and older builds. The legacy object never affected playback,
    /// so fields such as `prefer_hires` are not promoted to an invented user
    /// choice: they migrate to the historical effective behaviour, `max`.
    pub fn from_stored_json(raw: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Self::Max;
        };
        value
            .as_str()
            .or_else(|| value.get("quality").and_then(|q| q.as_str()))
            .and_then(Self::parse)
            .unwrap_or(Self::Max)
    }

    /// Provider token passed to `StreamingService::get_track_url`.
    ///
    /// `None` deliberately means "the service's maximum/default". Providers
    /// without a selectable quality (YouTube, Spotify Connect) also receive
    /// `None`; the resolved `StreamQuality` remains the source of truth and the
    /// orchestrator logs the unsupported preference explicitly.
    pub fn service_token(self, service: &str) -> Option<&'static str> {
        match (service, self) {
            (_, Self::Max) => None,
            ("qobuz", Self::Hires) => Some("hires"),
            ("qobuz", Self::Cd) => Some("cd"),
            ("qobuz", Self::Low) => Some("mp3"),
            ("tidal", Self::Hires) => Some("HI_RES_LOSSLESS"),
            ("tidal", Self::Cd) => Some("LOSSLESS"),
            ("tidal", Self::Low) => Some("HIGH"),
            ("deezer", Self::Hires | Self::Cd) => Some("FLAC"),
            ("deezer", Self::Low) => Some("MP3_320"),
            ("amazon", Self::Hires) => Some("ULTRA_HD"),
            ("amazon", Self::Cd) => Some("HD"),
            ("amazon", Self::Low) => Some("SD"),
            _ => None,
        }
    }

    pub fn provider_can_select(self, service: &str) -> bool {
        self == Self::Max || self.service_token(service).is_some()
    }

    /// Keep requested and delivered quality as two separate facts. A provider
    /// may legitimately fall back; copying the request into the result would
    /// make the selector look effective while lying about the bytes delivered.
    pub fn observe(self, service: &str, stream: &StreamUrl) -> StreamingQualityObservation {
        StreamingQualityObservation {
            requested: self.as_str(),
            provider_token: self.service_token(service).unwrap_or("provider_maximum"),
            delivered_codec: stream.quality.codec.clone(),
            delivered_sample_rate: stream.quality.sample_rate,
            delivered_bit_depth: stream.quality.bit_depth,
            delivered_bitrate: stream.quality.bitrate,
        }
    }
}

impl std::fmt::Display for StreamingQualityPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chaque_palier_est_traduit_par_service() {
        assert_eq!(
            StreamingQualityPreference::Hires.service_token("qobuz"),
            Some("hires")
        );
        assert_eq!(
            StreamingQualityPreference::Cd.service_token("tidal"),
            Some("LOSSLESS")
        );
        assert_eq!(
            StreamingQualityPreference::Low.service_token("deezer"),
            Some("MP3_320")
        );
        assert_eq!(
            StreamingQualityPreference::Hires.service_token("amazon"),
            Some("ULTRA_HD")
        );
    }

    #[test]
    fn une_ancienne_valeur_inerte_redevient_max_sans_inventer_un_choix() {
        assert_eq!(
            StreamingQualityPreference::from_stored_json(
                r#"{"max_sample_rate":96000,"max_bit_depth":24,"prefer_hires":false}"#,
            ),
            StreamingQualityPreference::Max
        );
        assert_eq!(
            StreamingQualityPreference::from_stored_json(r#"{"quality":"cd"}"#),
            StreamingQualityPreference::Cd
        );
    }

    #[test]
    fn un_service_sans_selecteur_est_annonce_comme_tel() {
        assert!(!StreamingQualityPreference::Cd.provider_can_select("youtube"));
        assert!(StreamingQualityPreference::Max.provider_can_select("youtube"));
    }

    #[test]
    fn la_preuve_distingue_la_demande_du_format_reellement_livre() {
        let delivered = StreamUrl {
            url: "https://cdn.example/track.flac".into(),
            mime_type: "audio/flac".into(),
            quality: super::super::traits::StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
        };

        let observation = StreamingQualityPreference::Hires.observe("tidal", &delivered);
        assert_eq!(observation.requested, "hires");
        assert_eq!(observation.provider_token, "HI_RES_LOSSLESS");
        assert_eq!(observation.delivered_sample_rate, 44_100);
        assert_eq!(observation.delivered_bit_depth, 16);
    }
}
