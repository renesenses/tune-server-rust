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
            "dsf" | "dff" | "dst" => Some(Self::Dsd),
            "wv" => Some(Self::WavPack),
            "ape" => Some(Self::Ape),
            _ => None,
        }
    }

    pub fn ffmpeg_format_arg(&self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Aac => "adts",
            Self::Alac => "ipod",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::Aiff => "aiff",
            Self::Dsd | Self::WavPack | Self::Ape => "wav",
        }
    }

    pub fn ffmpeg_codec_arg(&self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Wav => "pcm_s16le",
            Self::Mp3 => "libmp3lame",
            Self::Aac => "aac",
            Self::Alac => "alac",
            Self::Ogg => "libvorbis",
            Self::Opus => "libopus",
            Self::Aiff => "pcm_s16be",
            Self::Dsd | Self::WavPack | Self::Ape => "pcm_s16le",
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
        formats: vec![AudioFormat::Flac, AudioFormat::Wav, AudioFormat::Mp3, AudioFormat::Aac],
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

pub fn best_output_format(
    source_format: AudioFormat,
    source_sample_rate: u32,
    source_bit_depth: u16,
    target: &AudioCapabilities,
) -> AudioFormat {
    if can_passthrough(source_format, source_sample_rate, source_bit_depth, target) {
        return source_format;
    }
    let preferred = [AudioFormat::Flac, AudioFormat::Wav, AudioFormat::Aac, AudioFormat::Mp3];
    for fmt in preferred {
        if target.formats.contains(&fmt) {
            return fmt;
        }
    }
    *target.formats.first().unwrap_or(&AudioFormat::Wav)
}
