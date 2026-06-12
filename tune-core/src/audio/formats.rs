use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Flac,
    Wav,
    Mp3,
    Aac,
    Alac,
    Ogg,
    Opus,
    Aiff,
    Dsd,
    WavPack,
    Ape,
    Wma,
}

impl AudioFormat {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.trim_start_matches('.').to_lowercase().as_str() {
            "flac" => Some(Self::Flac),
            "wav" => Some(Self::Wav),
            "mp3" => Some(Self::Mp3),
            "m4a" | "aac" => Some(Self::Aac),
            "alac" => Some(Self::Alac),
            "ogg" | "oga" => Some(Self::Ogg),
            "opus" => Some(Self::Opus),
            "aiff" | "aif" => Some(Self::Aiff),
            "dsf" | "dff" | "dst" | "dsd" => Some(Self::Dsd),
            "wv" => Some(Self::WavPack),
            "ape" => Some(Self::Ape),
            "wma" | "asf" => Some(Self::Wma),
            _ => None,
        }
    }

    /// Container format identifier (e.g. "flac", "wav", "mp3").
    pub fn container_format(&self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Aac => "adts",
            Self::Alac => "ipod",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::Aiff => "aiff",
            Self::Dsd | Self::WavPack | Self::Ape | Self::Wma => "wav",
        }
    }

    /// Codec identifier (e.g. "flac", "pcm_s16le", "alac").
    pub fn codec_name(&self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Wav => "pcm_s16le",
            Self::Mp3 => "libmp3lame",
            Self::Aac => "aac",
            Self::Alac => "alac",
            Self::Ogg => "libvorbis",
            Self::Opus => "libopus",
            Self::Aiff => "pcm_s16be",
            Self::Dsd => "pcm_s24le",
            Self::WavPack | Self::Ape => "pcm_s24le",
            Self::Wma => "pcm_s16le",
        }
    }

    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Flac => "audio/flac",
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
            Self::Aac => "audio/aac",
            Self::Alac => "audio/mp4",
            Self::Ogg => "audio/ogg",
            Self::Opus => "audio/opus",
            Self::Aiff => "audio/aiff",
            Self::Dsd => "application/x-dsd",
            Self::WavPack => "audio/x-wavpack",
            Self::Ape => "audio/x-ape",
            Self::Wma => "audio/x-ms-wma",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioCapabilities {
    pub formats: Vec<AudioFormat>,
    pub max_sample_rate: u32,
    pub max_bit_depth: u16,
    pub supports_gapless: bool,
}

pub fn dlna_capabilities() -> AudioCapabilities {
    AudioCapabilities {
        formats: vec![
            AudioFormat::Flac,
            AudioFormat::Wav,
            AudioFormat::Mp3,
            AudioFormat::Aac,
        ],
        max_sample_rate: 192000,
        max_bit_depth: 24,
        supports_gapless: true,
    }
}

pub fn can_passthrough(
    source_format: AudioFormat,
    source_sample_rate: u32,
    source_bit_depth: u16,
    target: &AudioCapabilities,
) -> bool {
    target.formats.contains(&source_format)
        && source_sample_rate <= target.max_sample_rate
        && source_bit_depth <= target.max_bit_depth
}

impl AudioFormat {
    /// Returns true if this format is lossless (preserves all audio data).
    pub fn is_lossless(&self) -> bool {
        matches!(
            self,
            Self::Flac
                | Self::Wav
                | Self::Dsd
                | Self::Alac
                | Self::Aiff
                | Self::WavPack
                | Self::Ape
        )
    }

    /// Human-readable format name in uppercase (e.g. "FLAC", "WAV", "DSD").
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::Wav => "WAV",
            Self::Mp3 => "MP3",
            Self::Aac => "AAC",
            Self::Alac => "ALAC",
            Self::Ogg => "OGG",
            Self::Opus => "OPUS",
            Self::Aiff => "AIFF",
            Self::Dsd => "DSD",
            Self::WavPack => "WavPack",
            Self::Ape => "APE",
            Self::Wma => "WMA",
        }
    }

    /// Returns true if this format needs transcoding before DLNA streaming.
    /// FLAC, WAV, MP3, AAC can be served as raw files; everything else must be transcoded.
    pub fn needs_transcode_for_dlna(&self) -> bool {
        matches!(
            self,
            Self::Aiff | Self::Dsd | Self::WavPack | Self::Ape | Self::Alac | Self::Wma
        )
    }

    /// Returns the target output format for DLNA transcoding.
    /// AIFF -> FLAC (lossless PCM container widely supported by DLNA renderers)
    /// DSD/WavPack/APE -> WAV (universal PCM, avoids re-encoding overhead)
    pub fn dlna_transcode_target(&self) -> AudioFormat {
        match self {
            Self::Aiff => AudioFormat::Flac,
            Self::Alac | Self::Dsd | Self::WavPack | Self::Ape | Self::Wma => AudioFormat::Wav,
            other => *other,
        }
    }

    /// For DSD sources, compute the appropriate PCM output sample rate.
    /// DSD64 (2.8224 MHz) -> 176400 Hz (4x44100)
    /// DSD128 (5.6448 MHz) -> 352800 Hz (8x44100)
    /// DSD256 (11.2896 MHz) -> 352800 Hz (capped for compatibility)
    /// DSD512 (22.5792 MHz) -> 352800 Hz (capped for compatibility)
    /// For non-DSD formats, returns the source sample rate unchanged.
    pub fn dsd_output_sample_rate(&self, source_sample_rate: u32) -> u32 {
        if *self != Self::Dsd {
            return source_sample_rate;
        }
        // DSD sample rates are in the MHz range (e.g. 2_822_400 for DSD64)
        // Some scanners store them divided (e.g. 2822400 or 5644800)
        match source_sample_rate {
            r if r >= 11_000_000 => 352_800, // DSD256/512 -> cap at 352.8kHz
            r if r >= 5_000_000 => 352_800,  // DSD128 -> 352.8kHz
            r if r >= 2_000_000 => 176_400,  // DSD64 -> 176.4kHz
            // If scanner stored a lower value (some report 2822 or 5644), scale up
            r if r >= 5000 => 352_800, // DSD128-ish
            r if r >= 2000 => 176_400, // DSD64-ish
            // Fallback: safe default for DSD
            _ => 176_400,
        }
    }
}

pub fn best_output_format(
    source_format: AudioFormat,
    source_sample_rate: u32,
    source_bit_depth: u16,
    target: &AudioCapabilities,
) -> AudioFormat {
    if can_passthrough(source_format, source_sample_rate, source_bit_depth, target) {
        return source_format;
    }
    let preferred = [
        AudioFormat::Flac,
        AudioFormat::Wav,
        AudioFormat::Aac,
        AudioFormat::Mp3,
    ];
    for fmt in preferred {
        if target.formats.contains(&fmt) {
            return fmt;
        }
    }
    *target.formats.first().unwrap_or(&AudioFormat::Wav)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aiff_needs_transcode() {
        assert!(AudioFormat::Aiff.needs_transcode_for_dlna());
    }

    #[test]
    fn dsd_needs_transcode() {
        assert!(AudioFormat::Dsd.needs_transcode_for_dlna());
    }

    #[test]
    fn wavpack_needs_transcode() {
        assert!(AudioFormat::WavPack.needs_transcode_for_dlna());
    }

    #[test]
    fn ape_needs_transcode() {
        assert!(AudioFormat::Ape.needs_transcode_for_dlna());
    }

    #[test]
    fn wma_needs_transcode() {
        assert!(AudioFormat::Wma.needs_transcode_for_dlna());
    }

    #[test]
    fn flac_no_transcode() {
        assert!(!AudioFormat::Flac.needs_transcode_for_dlna());
    }

    #[test]
    fn wav_no_transcode() {
        assert!(!AudioFormat::Wav.needs_transcode_for_dlna());
    }

    #[test]
    fn mp3_no_transcode() {
        assert!(!AudioFormat::Mp3.needs_transcode_for_dlna());
    }

    #[test]
    fn aiff_transcodes_to_flac() {
        assert_eq!(AudioFormat::Aiff.dlna_transcode_target(), AudioFormat::Flac);
    }

    #[test]
    fn dsd_transcodes_to_wav() {
        assert_eq!(AudioFormat::Dsd.dlna_transcode_target(), AudioFormat::Wav);
    }

    #[test]
    fn wavpack_transcodes_to_wav() {
        assert_eq!(
            AudioFormat::WavPack.dlna_transcode_target(),
            AudioFormat::Wav
        );
    }

    #[test]
    fn ape_transcodes_to_wav() {
        assert_eq!(AudioFormat::Ape.dlna_transcode_target(), AudioFormat::Wav);
    }

    #[test]
    fn wma_transcodes_to_wav() {
        assert_eq!(AudioFormat::Wma.dlna_transcode_target(), AudioFormat::Wav);
    }

    #[test]
    fn flac_target_is_self() {
        assert_eq!(AudioFormat::Flac.dlna_transcode_target(), AudioFormat::Flac);
    }

    #[test]
    fn dsd64_sample_rate() {
        assert_eq!(AudioFormat::Dsd.dsd_output_sample_rate(2_822_400), 176_400);
    }

    #[test]
    fn dsd128_sample_rate() {
        assert_eq!(AudioFormat::Dsd.dsd_output_sample_rate(5_644_800), 352_800);
    }

    #[test]
    fn dsd256_sample_rate() {
        assert_eq!(AudioFormat::Dsd.dsd_output_sample_rate(11_289_600), 352_800);
    }

    #[test]
    fn non_dsd_passthrough_rate() {
        assert_eq!(AudioFormat::Flac.dsd_output_sample_rate(96000), 96000);
        assert_eq!(AudioFormat::Aiff.dsd_output_sample_rate(44100), 44100);
    }

    #[test]
    fn dsd_fallback_rate() {
        assert_eq!(AudioFormat::Dsd.dsd_output_sample_rate(0), 176_400);
    }

    #[test]
    fn dsd_codec_is_24bit() {
        assert_eq!(AudioFormat::Dsd.codec_name(), "pcm_s24le");
    }

    #[test]
    fn from_extension_dsf() {
        assert_eq!(AudioFormat::from_extension("dsf"), Some(AudioFormat::Dsd));
    }

    #[test]
    fn from_extension_dff() {
        assert_eq!(AudioFormat::from_extension("dff"), Some(AudioFormat::Dsd));
    }

    #[test]
    fn from_extension_aiff() {
        assert_eq!(AudioFormat::from_extension("aiff"), Some(AudioFormat::Aiff));
    }

    #[test]
    fn from_extension_aif() {
        assert_eq!(AudioFormat::from_extension("aif"), Some(AudioFormat::Aiff));
    }

    #[test]
    fn passthrough_flac() {
        let caps = dlna_capabilities();
        assert!(can_passthrough(AudioFormat::Flac, 96000, 24, &caps));
    }

    #[test]
    fn no_passthrough_aiff() {
        let caps = dlna_capabilities();
        assert!(!can_passthrough(AudioFormat::Aiff, 44100, 16, &caps));
    }

    #[test]
    fn no_passthrough_dsd() {
        let caps = dlna_capabilities();
        assert!(!can_passthrough(AudioFormat::Dsd, 2_822_400, 1, &caps));
    }

    #[test]
    fn best_output_for_aiff() {
        let caps = dlna_capabilities();
        let result = best_output_format(AudioFormat::Aiff, 44100, 16, &caps);
        assert_eq!(result, AudioFormat::Flac);
    }

    #[test]
    fn best_output_for_dsd() {
        let caps = dlna_capabilities();
        let result = best_output_format(AudioFormat::Dsd, 2_822_400, 1, &caps);
        assert_eq!(result, AudioFormat::Flac);
    }

    #[test]
    fn from_extension_flac() {
        assert_eq!(AudioFormat::from_extension("flac"), Some(AudioFormat::Flac));
        assert_eq!(AudioFormat::from_extension("FLAC"), Some(AudioFormat::Flac));
        assert_eq!(
            AudioFormat::from_extension(".flac"),
            Some(AudioFormat::Flac)
        );
    }

    #[test]
    fn from_extension_wav() {
        assert_eq!(AudioFormat::from_extension("wav"), Some(AudioFormat::Wav));
    }

    #[test]
    fn from_extension_mp3() {
        assert_eq!(AudioFormat::from_extension("mp3"), Some(AudioFormat::Mp3));
    }

    #[test]
    fn from_extension_m4a() {
        assert_eq!(AudioFormat::from_extension("m4a"), Some(AudioFormat::Aac));
        assert_eq!(AudioFormat::from_extension("aac"), Some(AudioFormat::Aac));
    }

    #[test]
    fn from_extension_ogg() {
        assert_eq!(AudioFormat::from_extension("ogg"), Some(AudioFormat::Ogg));
        assert_eq!(AudioFormat::from_extension("oga"), Some(AudioFormat::Ogg));
    }

    #[test]
    fn from_extension_opus() {
        assert_eq!(AudioFormat::from_extension("opus"), Some(AudioFormat::Opus));
    }

    #[test]
    fn from_extension_wavpack() {
        assert_eq!(
            AudioFormat::from_extension("wv"),
            Some(AudioFormat::WavPack)
        );
    }

    #[test]
    fn from_extension_ape() {
        assert_eq!(AudioFormat::from_extension("ape"), Some(AudioFormat::Ape));
    }

    #[test]
    fn from_extension_wma() {
        assert_eq!(AudioFormat::from_extension("wma"), Some(AudioFormat::Wma));
        assert_eq!(AudioFormat::from_extension("asf"), Some(AudioFormat::Wma));
    }

    #[test]
    fn from_extension_dst() {
        assert_eq!(AudioFormat::from_extension("dst"), Some(AudioFormat::Dsd));
    }

    #[test]
    fn from_extension_dsd_normalized() {
        // The metadata scanner normalizes "dsf"/"dff" to "dsd" in the DB.
        // AudioFormat::from_extension must recognise this normalised form
        // so the orchestrator correctly triggers DSD→PCM transcoding.
        assert_eq!(AudioFormat::from_extension("dsd"), Some(AudioFormat::Dsd));
    }

    #[test]
    fn from_extension_unknown() {
        assert!(AudioFormat::from_extension("txt").is_none());
        assert!(AudioFormat::from_extension("pdf").is_none());
        assert!(AudioFormat::from_extension("").is_none());
    }

    #[test]
    fn mime_types() {
        assert_eq!(AudioFormat::Flac.mime_type(), "audio/flac");
        assert_eq!(AudioFormat::Wav.mime_type(), "audio/wav");
        assert_eq!(AudioFormat::Mp3.mime_type(), "audio/mpeg");
        assert_eq!(AudioFormat::Aac.mime_type(), "audio/aac");
        assert_eq!(AudioFormat::Ogg.mime_type(), "audio/ogg");
        assert_eq!(AudioFormat::Opus.mime_type(), "audio/opus");
        assert_eq!(AudioFormat::Aiff.mime_type(), "audio/aiff");
        assert_eq!(AudioFormat::Dsd.mime_type(), "application/x-dsd");
        assert_eq!(AudioFormat::WavPack.mime_type(), "audio/x-wavpack");
        assert_eq!(AudioFormat::Ape.mime_type(), "audio/x-ape");
        assert_eq!(AudioFormat::Alac.mime_type(), "audio/mp4");
        assert_eq!(AudioFormat::Wma.mime_type(), "audio/x-ms-wma");
    }

    #[test]
    fn container_formats() {
        assert_eq!(AudioFormat::Flac.container_format(), "flac");
        assert_eq!(AudioFormat::Wav.container_format(), "wav");
        assert_eq!(AudioFormat::Mp3.container_format(), "mp3");
        assert_eq!(AudioFormat::Aac.container_format(), "adts");
        assert_eq!(AudioFormat::Alac.container_format(), "ipod");
        assert_eq!(AudioFormat::Ogg.container_format(), "ogg");
        assert_eq!(AudioFormat::Opus.container_format(), "opus");
        assert_eq!(AudioFormat::Aiff.container_format(), "aiff");
        assert_eq!(AudioFormat::Dsd.container_format(), "wav");
        assert_eq!(AudioFormat::WavPack.container_format(), "wav");
        assert_eq!(AudioFormat::Ape.container_format(), "wav");
        assert_eq!(AudioFormat::Wma.container_format(), "wav");
    }

    #[test]
    fn codec_names() {
        assert_eq!(AudioFormat::Flac.codec_name(), "flac");
        assert_eq!(AudioFormat::Wav.codec_name(), "pcm_s16le");
        assert_eq!(AudioFormat::Mp3.codec_name(), "libmp3lame");
        assert_eq!(AudioFormat::Aac.codec_name(), "aac");
        assert_eq!(AudioFormat::Alac.codec_name(), "alac");
        assert_eq!(AudioFormat::Ogg.codec_name(), "libvorbis");
        assert_eq!(AudioFormat::Opus.codec_name(), "libopus");
        assert_eq!(AudioFormat::Aiff.codec_name(), "pcm_s16be");
        assert_eq!(AudioFormat::WavPack.codec_name(), "pcm_s24le");
        assert_eq!(AudioFormat::Ape.codec_name(), "pcm_s24le");
        assert_eq!(AudioFormat::Wma.codec_name(), "pcm_s16le");
    }

    #[test]
    fn dlna_capabilities_check() {
        let caps = dlna_capabilities();
        assert!(caps.formats.contains(&AudioFormat::Flac));
        assert!(caps.formats.contains(&AudioFormat::Wav));
        assert!(caps.formats.contains(&AudioFormat::Mp3));
        assert!(caps.formats.contains(&AudioFormat::Aac));
        assert!(!caps.formats.contains(&AudioFormat::Ogg));
        assert_eq!(caps.max_sample_rate, 192000);
        assert_eq!(caps.max_bit_depth, 24);
        assert!(caps.supports_gapless);
    }

    #[test]
    fn passthrough_wav() {
        let caps = dlna_capabilities();
        assert!(can_passthrough(AudioFormat::Wav, 44100, 16, &caps));
        assert!(can_passthrough(AudioFormat::Wav, 192000, 24, &caps));
    }

    #[test]
    fn no_passthrough_over_max_rate() {
        let caps = dlna_capabilities();
        assert!(!can_passthrough(AudioFormat::Flac, 384000, 24, &caps));
    }

    #[test]
    fn best_output_for_wavpack() {
        let caps = dlna_capabilities();
        let result = best_output_format(AudioFormat::WavPack, 44100, 16, &caps);
        assert_eq!(result, AudioFormat::Flac);
    }

    #[test]
    fn best_output_for_ape() {
        let caps = dlna_capabilities();
        let result = best_output_format(AudioFormat::Ape, 96000, 24, &caps);
        assert_eq!(result, AudioFormat::Flac);
    }

    #[test]
    fn passthrough_mp3() {
        let caps = dlna_capabilities();
        assert!(can_passthrough(AudioFormat::Mp3, 44100, 16, &caps));
    }

    #[test]
    fn audio_format_serialization() {
        let fmt = AudioFormat::Flac;
        let json = serde_json::to_string(&fmt).unwrap();
        assert_eq!(json, "\"flac\"");

        let deserialized: AudioFormat = serde_json::from_str("\"mp3\"").unwrap();
        assert_eq!(deserialized, AudioFormat::Mp3);
    }

    #[test]
    fn audio_format_equality() {
        assert_eq!(AudioFormat::Flac, AudioFormat::Flac);
        assert_ne!(AudioFormat::Flac, AudioFormat::Mp3);
    }

    #[test]
    fn dsd512_sample_rate() {
        assert_eq!(AudioFormat::Dsd.dsd_output_sample_rate(22_579_200), 352_800);
    }
}
