//! Multichannel audio support: layout enum, device detection, downmix matrices.
//!
//! Ported from Python `tune_server/audio/formats.py` (feat/multichannel branch).
//! Supports up to 16 channels (9.1.6 Auro-3D).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Channel layout enum
// ---------------------------------------------------------------------------

/// Standard channel layouts for audio content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    /// 1 channel
    Mono,
    /// 2 channels (L, R)
    Stereo,
    /// 6 channels (L, R, C, LFE, Ls, Rs)
    Surround51,
    /// 8 channels (L, R, C, LFE, Ls, Rs, Lb, Rb)
    Surround71,
    /// 10 channels (L, R, C, LFE, Ls, Rs, Lb, Rb, Ltf, Rtf) — 5.1.4
    Surround514,
    /// 12 channels (L, R, C, LFE, Ls, Rs, Lb, Rb, Ltf, Rtf, Ltr, Rtr) — 7.1.4 Atmos
    Surround714,
    /// 16 channels — 9.1.6 Auro-3D
    Surround916,
}

impl ChannelLayout {
    /// Number of discrete channels in this layout.
    pub fn channel_count(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Surround51 => 6,
            Self::Surround71 => 8,
            Self::Surround514 => 10,
            Self::Surround714 => 12,
            Self::Surround916 => 16,
        }
    }

    /// Infer layout from a raw channel count.
    pub fn from_channel_count(channels: u16) -> Self {
        match channels {
            0 | 1 => Self::Mono,
            2 => Self::Stereo,
            3..=6 => Self::Surround51,
            7 | 8 => Self::Surround71,
            9 | 10 => Self::Surround514,
            11 | 12 => Self::Surround714,
            _ => Self::Surround916,
        }
    }

    /// Human-readable badge string for UI display (e.g. "5.1", "7.1.4 Atmos").
    /// Returns `None` for mono/stereo (no badge needed).
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Self::Mono | Self::Stereo => None,
            Self::Surround51 => Some("5.1"),
            Self::Surround71 => Some("7.1"),
            Self::Surround514 => Some("5.1.4"),
            Self::Surround714 => Some("7.1.4 Atmos"),
            Self::Surround916 => Some("9.1.6 Auro-3D"),
        }
    }

    /// Returns true if this layout has more than 2 channels.
    pub fn is_multichannel(self) -> bool {
        self.channel_count() > 2
    }
}

/// Return a badge string for a given channel count.
/// Returns `None` for mono/stereo.
pub fn channel_badge(channels: u16) -> Option<&'static str> {
    ChannelLayout::from_channel_count(channels).badge()
}

// ---------------------------------------------------------------------------
// Device channel detection — heuristic from known brands
// ---------------------------------------------------------------------------

/// Known multichannel-capable device name/model patterns (case-insensitive)
/// with their maximum supported channel count.
const MULTICHANNEL_CAPABLE_PATTERNS: &[(&str, u16)] = &[
    ("marantz", 8),
    ("denon", 8),
    ("yamaha", 8),
    ("pioneer", 8),
    ("onkyo", 8),
    ("nad", 8),
    ("anthem", 8),
    ("arcam", 8),
    ("sonos arc", 6),
    ("sonos beam", 6),
    ("samsung", 6),
];

/// Heuristic: check device name/model against known multichannel-capable devices.
/// Returns the max channel count, or `None` if unknown.
pub fn detect_max_channels_from_device_name(name: &str) -> Option<u16> {
    let lower = name.to_lowercase();
    for &(pattern, channels) in MULTICHANNEL_CAPABLE_PATTERNS {
        if lower.contains(pattern) {
            return Some(channels);
        }
    }
    None
}

/// Detect max channel count by checking both device name and model strings.
pub fn detect_max_channels_from_device_info(name: &str, model: &str) -> Option<u16> {
    let combined = format!("{name} {model}").to_lowercase();
    for &(pattern, channels) in MULTICHANNEL_CAPABLE_PATTERNS {
        if combined.contains(pattern) {
            return Some(channels);
        }
    }
    None
}

/// Parse DLNA sink protocol entries for max channel count.
///
/// Protocol entries may contain `channels=N` (e.g.
/// `http-get:*:audio/flac:*;channels=6`). Returns the maximum found,
/// or 2 (stereo) if nothing is detected.
pub fn detect_max_channels_from_sink_protocols(sink_protocols: &[String]) -> u16 {
    let mut max_ch: u16 = 2;
    for entry in sink_protocols {
        let lower = entry.to_lowercase();
        // Look for "channels=N" anywhere in the protocol entry
        if let Some(pos) = lower.find("channels=") {
            let after = &lower[pos + 9..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u16>() {
                max_ch = max_ch.max(n);
            }
        }
    }
    max_ch
}

// ---------------------------------------------------------------------------
// Downmix matrix — ITU-R BS.775 standard coefficients
// ---------------------------------------------------------------------------

/// ITU-R BS.775 coefficient for center channel in stereo downmix.
const ITU_CENTER_COEFF: f32 = 0.707; // -3 dB (1/sqrt(2))

/// ITU-R BS.775 coefficient for surround channels in stereo downmix.
const ITU_SURROUND_COEFF: f32 = 0.707; // -3 dB

/// ITU-R BS.775 coefficient for surround channels in mono downmix.
const ITU_SURROUND_MONO_COEFF: f32 = 0.354; // -9 dB (0.5 * 0.707)

/// Build a downmix coefficient matrix for converting `source_ch` channels
/// to `target_ch` channels.
///
/// Returns `None` if no downmix is needed (source <= target).
///
/// The returned vector has `target_ch * source_ch` elements, laid out row-major:
/// `output[out_ch * source_ch + in_ch]` = coefficient to apply to input
/// channel `in_ch` when computing output channel `out_ch`.
///
/// Standard 5.1 channel order: FL, FR, FC, LFE, BL, BR
/// Standard 7.1 channel order: FL, FR, FC, LFE, BL, BR, SL, SR
pub fn build_downmix_matrix(source_ch: u16, target_ch: u16) -> Option<Vec<f32>> {
    if source_ch <= target_ch {
        return None;
    }

    let src = source_ch as usize;
    let tgt = target_ch as usize;
    let mut matrix = vec![0.0f32; tgt * src];

    match (source_ch, target_ch) {
        // 5.1 (6ch) -> stereo (2ch): ITU-R BS.775
        // L_out = FL + 0.707*FC + 0.707*BL
        // R_out = FR + 0.707*FC + 0.707*BR
        (6, 2) => {
            // Row 0 (left output): FL=1.0, FC=0.707, BL=0.707
            matrix[0] = 1.0; // FL
            matrix[2] = ITU_CENTER_COEFF; // FC
            matrix[4] = ITU_SURROUND_COEFF; // BL
            // Row 1 (right output): FR=1.0, FC=0.707, BR=0.707
            matrix[src + 1] = 1.0; // FR
            matrix[src + 2] = ITU_CENTER_COEFF; // FC
            matrix[src + 5] = ITU_SURROUND_COEFF; // BR
        }

        // 7.1 (8ch) -> stereo (2ch): extended ITU-R BS.775
        // L_out = FL + 0.707*FC + 0.707*BL + 0.707*SL
        // R_out = FR + 0.707*FC + 0.707*BR + 0.707*SR
        (8, 2) => {
            matrix[0] = 1.0; // FL
            matrix[2] = ITU_CENTER_COEFF; // FC
            matrix[4] = ITU_SURROUND_COEFF; // BL
            matrix[6] = ITU_SURROUND_COEFF; // SL
            matrix[src + 1] = 1.0; // FR
            matrix[src + 2] = ITU_CENTER_COEFF; // FC
            matrix[src + 5] = ITU_SURROUND_COEFF; // BR
            matrix[src + 7] = ITU_SURROUND_COEFF; // SR
        }

        // Any multichannel (>=6) -> mono (1ch)
        (s, 1) if s >= 6 => {
            // Mono = 0.5*FL + 0.5*FR + 0.707*FC + 0.354*BL + 0.354*BR
            matrix[0] = 0.5; // FL
            matrix[1] = 0.5; // FR
            matrix[2] = ITU_CENTER_COEFF; // FC
            // LFE (index 3) intentionally excluded from mono downmix
            if src > 4 {
                matrix[4] = ITU_SURROUND_MONO_COEFF; // BL
            }
            if src > 5 {
                matrix[5] = ITU_SURROUND_MONO_COEFF; // BR
            }
        }

        // 5.1 (6ch) -> mono (1ch)
        (6, 1) => {
            matrix[0] = 0.5;
            matrix[1] = 0.5;
            matrix[2] = ITU_CENTER_COEFF;
            matrix[4] = ITU_SURROUND_MONO_COEFF;
            matrix[5] = ITU_SURROUND_MONO_COEFF;
        }

        // 7.1 (8ch) -> 5.1 (6ch): fold side channels into rears
        (8, 6) => {
            // Pass through FL, FR, FC, LFE, fold SL+BL -> BL, SR+BR -> BR
            for i in 0..6 {
                matrix[i * src + i] = 1.0; // identity for first 6
            }
            // Add side left to back left
            matrix[4 * src + 6] = ITU_SURROUND_COEFF; // SL -> BL
            // Add side right to back right
            matrix[5 * src + 7] = ITU_SURROUND_COEFF; // SR -> BR
        }

        // Generic fallback: pass through first `target_ch` channels
        _ => {
            for i in 0..tgt.min(src) {
                matrix[i * src + i] = 1.0;
            }
        }
    }

    Some(matrix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_layout_counts() {
        assert_eq!(ChannelLayout::Mono.channel_count(), 1);
        assert_eq!(ChannelLayout::Stereo.channel_count(), 2);
        assert_eq!(ChannelLayout::Surround51.channel_count(), 6);
        assert_eq!(ChannelLayout::Surround71.channel_count(), 8);
        assert_eq!(ChannelLayout::Surround514.channel_count(), 10);
        assert_eq!(ChannelLayout::Surround714.channel_count(), 12);
        assert_eq!(ChannelLayout::Surround916.channel_count(), 16);
    }

    #[test]
    fn channel_layout_from_count() {
        assert_eq!(ChannelLayout::from_channel_count(1), ChannelLayout::Mono);
        assert_eq!(ChannelLayout::from_channel_count(2), ChannelLayout::Stereo);
        assert_eq!(
            ChannelLayout::from_channel_count(6),
            ChannelLayout::Surround51
        );
        assert_eq!(
            ChannelLayout::from_channel_count(8),
            ChannelLayout::Surround71
        );
        assert_eq!(
            ChannelLayout::from_channel_count(12),
            ChannelLayout::Surround714
        );
    }

    #[test]
    fn channel_badges() {
        assert_eq!(channel_badge(1), None);
        assert_eq!(channel_badge(2), None);
        assert_eq!(channel_badge(6), Some("5.1"));
        assert_eq!(channel_badge(8), Some("7.1"));
        assert_eq!(channel_badge(12), Some("7.1.4 Atmos"));
        assert_eq!(channel_badge(16), Some("9.1.6 Auro-3D"));
    }

    #[test]
    fn detect_from_device_name() {
        assert_eq!(
            detect_max_channels_from_device_name("Marantz SR7009"),
            Some(8)
        );
        assert_eq!(
            detect_max_channels_from_device_name("Denon AVR-X3700H"),
            Some(8)
        );
        assert_eq!(detect_max_channels_from_device_name("Sonos Arc"), Some(6));
        assert_eq!(detect_max_channels_from_device_name("Unknown Device"), None);
    }

    #[test]
    fn detect_from_device_info() {
        assert_eq!(
            detect_max_channels_from_device_info("Living Room", "Yamaha RX-A2080"),
            Some(8)
        );
        assert_eq!(
            detect_max_channels_from_device_info("MyDevice", "CustomModel"),
            None
        );
    }

    #[test]
    fn detect_from_sink_protocols() {
        let protos = vec![
            "http-get:*:audio/flac:*".to_string(),
            "http-get:*:audio/wav:*;channels=6".to_string(),
        ];
        assert_eq!(detect_max_channels_from_sink_protocols(&protos), 6);

        let protos_8 = vec![
            "http-get:*:audio/flac:*;channels=8".to_string(),
            "http-get:*:audio/wav:*;channels=2".to_string(),
        ];
        assert_eq!(detect_max_channels_from_sink_protocols(&protos_8), 8);

        let protos_none = vec!["http-get:*:audio/flac:*".to_string()];
        assert_eq!(detect_max_channels_from_sink_protocols(&protos_none), 2);
    }

    #[test]
    fn no_downmix_when_not_needed() {
        assert!(build_downmix_matrix(2, 2).is_none());
        assert!(build_downmix_matrix(2, 6).is_none());
        assert!(build_downmix_matrix(1, 2).is_none());
    }

    #[test]
    fn downmix_51_to_stereo() {
        let matrix = build_downmix_matrix(6, 2).unwrap();
        assert_eq!(matrix.len(), 12); // 2 * 6
        // Left channel: FL=1.0, FC=0.707, BL=0.707
        assert!((matrix[0] - 1.0).abs() < 0.001);
        assert!((matrix[2] - 0.707).abs() < 0.001);
        assert!((matrix[4] - 0.707).abs() < 0.001);
        // Right channel: FR=1.0, FC=0.707, BR=0.707
        assert!((matrix[7] - 1.0).abs() < 0.001);
        assert!((matrix[8] - 0.707).abs() < 0.001);
        assert!((matrix[11] - 0.707).abs() < 0.001);
    }

    #[test]
    fn downmix_71_to_stereo() {
        let matrix = build_downmix_matrix(8, 2).unwrap();
        assert_eq!(matrix.len(), 16); // 2 * 8
        // Left: FL=1.0, FC=0.707, BL=0.707, SL=0.707
        assert!((matrix[0] - 1.0).abs() < 0.001);
        assert!((matrix[2] - 0.707).abs() < 0.001);
        assert!((matrix[4] - 0.707).abs() < 0.001);
        assert!((matrix[6] - 0.707).abs() < 0.001);
    }

    #[test]
    fn downmix_71_to_51() {
        let matrix = build_downmix_matrix(8, 6).unwrap();
        assert_eq!(matrix.len(), 48); // 6 * 8
        // First 4 channels pass through (FL, FR, FC, LFE)
        assert!((matrix[0] - 1.0).abs() < 0.001); // FL->FL
        assert!((matrix[8 + 1] - 1.0).abs() < 0.001); // FR->FR
        assert!((matrix[16 + 2] - 1.0).abs() < 0.001); // FC->FC
        assert!((matrix[24 + 3] - 1.0).abs() < 0.001); // LFE->LFE
        // BL output: BL=1.0 + SL*0.707
        assert!((matrix[32 + 4] - 1.0).abs() < 0.001); // BL->BL
        assert!((matrix[32 + 6] - 0.707).abs() < 0.001); // SL->BL
    }

    #[test]
    fn downmix_51_to_mono() {
        let matrix = build_downmix_matrix(6, 1).unwrap();
        assert_eq!(matrix.len(), 6); // 1 * 6
        assert!((matrix[0] - 0.5).abs() < 0.001); // FL
        assert!((matrix[1] - 0.5).abs() < 0.001); // FR
        assert!((matrix[2] - 0.707).abs() < 0.001); // FC
        assert!((matrix[3]).abs() < 0.001); // LFE excluded
        assert!((matrix[4] - 0.354).abs() < 0.001); // BL
        assert!((matrix[5] - 0.354).abs() < 0.001); // BR
    }

    #[test]
    fn is_multichannel() {
        assert!(!ChannelLayout::Mono.is_multichannel());
        assert!(!ChannelLayout::Stereo.is_multichannel());
        assert!(ChannelLayout::Surround51.is_multichannel());
        assert!(ChannelLayout::Surround71.is_multichannel());
        assert!(ChannelLayout::Surround714.is_multichannel());
    }

    #[test]
    fn generic_fallback_downmix() {
        // 10ch -> 2ch should use generic fallback (pass through first 2)
        let matrix = build_downmix_matrix(10, 2).unwrap();
        assert_eq!(matrix.len(), 20); // 2 * 10
        assert!((matrix[0] - 1.0).abs() < 0.001); // ch0 -> out0
        assert!((matrix[11] - 1.0).abs() < 0.001); // ch1 -> out1
    }
}
