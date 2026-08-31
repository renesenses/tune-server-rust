pub mod aac_encoder;
pub mod aiff;
pub mod alac_encoder;
pub mod analyzer;
pub mod ape;
pub mod audiophile;
pub mod channels;
pub mod convolver;
pub mod crossfeed;
pub mod dash_growth;
pub mod decode;
pub mod dff;
pub mod dsd_to_dop;
pub mod dsd_to_pcm;
pub mod dsf;
#[cfg(feature = "audio-embedding")]
pub mod embedding;
/// READ side of audio embeddings (storage + cosine) — always compiled, no ort.
pub mod embedding_store;
pub mod encoder;
pub mod eq;
pub mod eq_presets;
pub mod faststart;
pub mod formats;
pub mod http_range;
pub mod iso_sacd;
pub mod levels;
pub mod m4a;
pub mod mixer;
pub mod opus_ogg;
pub mod pipeline;
pub mod replaygain;
pub mod resample;
/// Rampe de gain anti-« ploc » à la pause / reprise / arrêt (#1590).
pub mod soft_mute;
/// Runtime provisioning of the onnxruntime shared lib (`load-dynamic`).
#[cfg(feature = "audio-embedding")]
pub mod runtime;
pub mod staged_growth;
pub mod support;
pub mod tap;
/// CLAP text tower for natural-language acoustic search (Phase 3).
#[cfg(feature = "audio-embedding")]
pub mod text_embedding;
pub mod thermal;
/// Conversion volume linéaire ↔ décibels — le seul endroit (#1274).
pub mod volume_scale;
pub mod wav;
pub mod wavpack;

/// Linear (2-tap) resampler over interleaved f32 samples.
///
/// Cheap, stateless, and always compiled (no `local-audio` gate) so it can be
/// shared by the local-output pipeline and the radio→WAV proxy that feeds DLNA
/// renderers. Quality is sufficient for lossy sources (e.g. HE-AAC radio
/// upsampled 22050→44100); the local pipeline prefers the rubato sinc path for
/// hi-res material and only falls back here.
pub(crate) fn simple_resample(
    samples: &[f32],
    from_sr: u32,
    to_sr: u32,
    channels: u16,
) -> Vec<f32> {
    if from_sr == to_sr {
        return samples.to_vec();
    }
    let ch = channels as usize;
    if ch == 0 {
        return Vec::new();
    }
    let in_frames = samples.len() / ch;
    if in_frames == 0 {
        return Vec::new();
    }
    let ratio = to_sr as f64 / from_sr as f64;
    let out_frames = (in_frames as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(out_frames * ch);

    for i in 0..out_frames {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;
        let idx0 = idx.min(in_frames - 1);
        let idx1 = (idx + 1).min(in_frames - 1);
        for c in 0..ch {
            let s0 = samples[idx0 * ch + c];
            let s1 = samples[idx1 * ch + c];
            out.push(s0 + (s1 - s0) * frac);
        }
    }
    out
}
