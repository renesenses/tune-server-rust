//! Audio-source boundary. Providers own authentication/decoding; Tune owns
//! HTTP sessions, renderers, measurement and playback lifetime.
use crate::{audio::formats::AudioFormat, streaming::StreamTrack};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioDelivery {
    /// Existing get_track_url contract (including its quality/auth retry).
    #[default]
    Url,
    /// Signed interleaved little-endian PCM. Seeking reopens the producer.
    DecodedPcm,
}

pub struct PcmRequest<'a> {
    pub source_id: &'a str,
    pub zone_id: i64,
    pub seek_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
}

impl PcmFormat {
    pub fn validate(self) -> Result<(), String> {
        if !(8000..=192000).contains(&self.sample_rate)
            || ![16, 24].contains(&self.bit_depth)
            || ![1, 2].contains(&self.channels)
        {
            return Err("Unsupported decoded PCM format".into());
        }
        Ok(())
    }

    pub fn frame_bytes(self) -> usize {
        usize::from(self.bit_depth / 8) * usize::from(self.channels)
    }
}

/// Bounded producer. A clean channel EOF means successful decoding; errors
/// must be sent before closing. Chunks need not end on a sample/frame boundary.
/// Providers MUST select closed() alongside blocked reads and reap resources
/// when Tune drops the source (stop, replacement or failed startup).
pub struct PcmSender(mpsc::Sender<Result<Vec<u8>, String>>);

pub const MAX_PCM_CHUNK_BYTES: usize = 64 * 1024;
const PCM_QUEUE_CHUNKS: usize = 8;

impl PcmSender {
    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.len() > MAX_PCM_CHUNK_BYTES {
            return Err("Decoded PCM chunk exceeds limit".into());
        }
        self.0
            .send(Ok(bytes))
            .await
            .map_err(|_| "PCM consumer closed".into())
    }

    pub async fn fail(&self, error: String) {
        let _ = self.0.send(Err(error)).await;
    }

    pub async fn closed(&self) {
        self.0.closed().await;
    }
}

pub struct DecodedPcmSource {
    pub format: PcmFormat,
    /// Observed by the decoder, never inferred from requested quality.
    pub source_format: AudioFormat,
    pub track: StreamTrack,
    pub(crate) pcm: mpsc::Receiver<Result<Vec<u8>, String>>,
}

impl DecodedPcmSource {
    pub fn channel(
        format: PcmFormat,
        source_format: AudioFormat,
        track: StreamTrack,
    ) -> Result<(PcmSender, Self), String> {
        format.validate()?;
        let (tx, pcm) = mpsc::channel(PCM_QUEUE_CHUNKS);
        Ok((
            PcmSender(tx),
            Self {
                format,
                source_format,
                track,
                pcm,
            },
        ))
    }
}
