//! Integer FLAC -> PCM, preserving the decoded source precision. No audio device.
use super::super::audio::{AudioHeader, PcmFormat};
use std::io::{Read, SeekFrom, Write};
use symphonia::core::{
    codecs::{
        CodecParameters,
        audio::{AudioDecoderOptions, well_known::CODEC_ID_FLAC},
    },
    formats::{FormatOptions, SeekMode, SeekTo, TrackType, probe::Hint},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    units::Time,
};
use tune_core::streaming::StreamTrack;

fn streaminfo(bytes: &[u8; 42], expected_bits: u16) -> Result<(PcmFormat, u64), String> {
    if &bytes[..4] != b"fLaC" || bytes[4] & 127 != 0 || bytes[5..8] != [0, 0, 34] {
        return Err("Spotify key/file did not produce a FLAC STREAMINFO header".into());
    }
    let min = u16::from_be_bytes(bytes[8..10].try_into().unwrap());
    let max = u16::from_be_bytes(bytes[10..12].try_into().unwrap());
    let packed = u64::from_be_bytes(bytes[18..26].try_into().unwrap());
    let pcm = PcmFormat {
        sample_rate: (packed >> 44) as u32,
        channels: ((packed >> 41) as u16 & 7) + 1,
        bit_depth: ((packed >> 36) as u16 & 31) + 1,
    };
    let frames = packed & ((1 << 36) - 1);
    pcm.validate()?;
    if min < 16 || min > max || frames == 0 || pcm.bit_depth != expected_bits {
        return Err("Spotify FLAC parameters do not match the requested quality".into());
    }
    Ok((pcm, frames))
}

pub(super) fn run(
    source: Box<dyn MediaSource>,
    bits: u16,
    seek_ms: u32,
    track: StreamTrack,
    published: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    decode(
        source,
        bits,
        seek_ms,
        |pcm| {
            let header: Result<AudioHeader, super::super::ipc::Failure> = Ok(AudioHeader {
                track,
                pcm,
                source_codec: "flac".into(),
            });
            let bytes = serde_json::to_vec(&header)
                .map_err(|_| "Spotify audio header serialization failed")?;
            let mut out = std::io::stdout();
            out.write_all(&(bytes.len() as u32).to_be_bytes())
                .map_err(|_| "Spotify PCM pipe closed")?;
            out.write_all(&bytes)
                .map_err(|_| "Spotify PCM pipe closed")?;
            out.flush()
                .map_err(|_| "Spotify PCM pipe closed".to_owned())?;
            published.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        },
        &mut stdout,
    )
}

fn decode(
    mut source: Box<dyn MediaSource>,
    bits: u16,
    seek_ms: u32,
    ready: impl FnOnce(PcmFormat) -> Result<(), String>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let mut header = [0u8; 42];
    source
        .read_exact(&mut header)
        .map_err(|_| "Spotify FLAC header is truncated")?;
    let (pcm, total) = streaminfo(&header, bits)?;
    let target = u64::from(seek_ms) * u64::from(pcm.sample_rate) / 1000;
    if target >= total {
        return Err("Spotify seek is outside decoded FLAC duration".into());
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|_| "Spotify FLAC rewind failed")?;
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|_| "Spotify FLAC probe failed")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("Spotify FLAC has no audio track")?;
    let id = track.id;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("Spotify FLAC has no audio parameters".into()),
    };
    if params.codec != CODEC_ID_FLAC
        || params.sample_rate != Some(pcm.sample_rate)
        || params.bits_per_sample != Some(u32::from(pcm.bit_depth))
        || params.channels.as_ref().map(|c| c.count()) != Some(usize::from(pcm.channels))
    {
        return Err("Spotify FLAC decoder parameters differ from STREAMINFO".into());
    }
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default().verify(target == 0))
        .map_err(|_| "Spotify FLAC decoder failed")?;
    let mut skip = 0;
    let mut next_frame = 0;
    if target > 0 {
        let time = Time::try_new(i64::from(seek_ms / 1000), (seek_ms % 1000) * 1_000_000)
            .ok_or("Spotify seek time is invalid")?;
        let reached = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(id),
                },
            )
            .map_err(|_| "Spotify FLAC accurate seek failed")?;
        let actual = u64::try_from(reached.actual_ts.get())
            .map_err(|_| "Spotify seek returned a negative position")?;
        if actual > target {
            return Err("Spotify seek overshot the requested sample".into());
        }
        skip = target - actual;
        next_frame = actual;
        decoder.reset();
    }
    let mut ready = Some(ready);
    let mut emitted = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => return Err("Spotify FLAC packet is invalid".into()),
        };
        if packet.track_id != id {
            return Err("Spotify FLAC unexpectedly changed tracks".into());
        }
        if u64::try_from(packet.pts.get()).ok() != Some(next_frame) {
            return Err("Spotify FLAC has a missing or repeated frame".into());
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|_| "Spotify FLAC frame failed integrity checks")?;
        if decoded.spec().rate() != pcm.sample_rate
            || decoded.spec().channels().count() != usize::from(pcm.channels)
        {
            return Err("Spotify FLAC changed PCM format midstream".into());
        }
        next_frame += decoded.frames() as u64;
        if next_frame > total {
            return Err("Spotify FLAC exceeds its declared sample count".into());
        }
        let mut samples = Vec::new();
        decoded.copy_to_vec_interleaved::<i32>(&mut samples);
        let frames_to_skip = skip.min(decoded.frames() as u64);
        skip -= frames_to_skip;
        let samples = &samples[frames_to_skip as usize * usize::from(pcm.channels)..];
        if samples.is_empty() {
            continue;
        }
        // Header follows key/header/decoder/seek validation AND a real frame.
        if let Some(ready) = ready.take() {
            ready(pcm)?;
        }
        let width = usize::from(pcm.bit_depth / 8);
        let mut bytes = Vec::with_capacity(samples.len() * width);
        for sample in samples {
            // Symphonia returns left-justified i32; retain all 16/24 source bits.
            let value = sample >> (32 - pcm.bit_depth);
            bytes.extend_from_slice(&value.to_le_bytes()[..width]);
        }
        for chunk in bytes.chunks(1024 * pcm.frame_bytes()) {
            out.write_all(chunk)
                .map_err(|_| "Spotify PCM pipe closed")?;
        }
        emitted += (samples.len() / usize::from(pcm.channels)) as u64;
    }
    if skip != 0 || emitted != total - target || ready.is_some() {
        return Err("Spotify FLAC ended before the declared sample count".into());
    }
    if target == 0 && decoder.finalize().verify_ok == Some(false) {
        return Err("Spotify FLAC audio checksum failed".into());
    }
    out.flush().map_err(|_| "Spotify PCM pipe closed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::audio::encoder::AudioEncoder;
    fn fixture(bits: u16, rate: u32) -> (Vec<u8>, Vec<u8>) {
        let mut pcm = Vec::new();
        for n in 0..24002i32 {
            let value = (n.wrapping_mul(701) % (1 << (bits - 1))) - (1 << (bits - 2));
            pcm.extend_from_slice(&value.to_le_bytes()[..usize::from(bits / 8)]);
        }
        let mut encoder = AudioEncoder::new("flac", rate, u32::from(bits), 2);
        encoder.start_sync().unwrap();
        encoder.write_sync(&pcm).unwrap();
        (encoder.finish_sync().unwrap(), pcm)
    }
    #[test]
    fn native_lossless_flac_roundtrip_retains_every_16_and_24_bit_sample() {
        for (bits, rate) in [(16, 44100), (24, 48000), (24, 96000)] {
            let (flac, expected) = fixture(bits, rate);
            let mut pcm = Vec::new();
            decode(
                Box::new(std::io::Cursor::new(flac)),
                bits,
                0,
                |format| {
                    assert_eq!(
                        format,
                        PcmFormat {
                            bit_depth: bits,
                            sample_rate: rate,
                            channels: 2
                        }
                    );
                    Ok(())
                },
                &mut pcm,
            )
            .unwrap();
            assert!(
                pcm == expected,
                "Lossless decode must retain every source bit ({bits} bit / {rate} Hz)"
            );
        }
    }
    #[test]
    fn native_lossless_seek_is_sample_exact_not_just_a_new_clock() {
        for bits in [16, 24] {
            let (flac, expected) = fixture(bits, 44100);
            let mut pcm = Vec::new();
            decode(
                Box::new(std::io::Cursor::new(flac)),
                bits,
                101,
                |_| Ok(()),
                &mut pcm,
            )
            .unwrap();
            let start = (101 * 44100 / 1000) * 2 * usize::from(bits / 8);
            assert!(
                pcm == expected[start..],
                "Seek must trim to the requested source sample"
            );
        }
    }
    #[test]
    fn native_lossless_wrong_key_truncation_and_wrong_quality_are_refused() {
        let (flac, _) = fixture(16, 44100);
        for (data, bits) in [
            (vec![0; 100], 16),
            (flac[..flac.len() - 100].to_vec(), 16),
            (flac, 24),
        ] {
            assert!(
                decode(
                    Box::new(std::io::Cursor::new(data)),
                    bits,
                    0,
                    |_| Ok(()),
                    &mut Vec::new()
                )
                .is_err()
            );
        }
    }
}
