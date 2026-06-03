use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tracing::debug;

pub struct DecodedAudio {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration_s: f64,
}

pub fn can_decode_native(file_path: &str) -> bool {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "flac" | "mp3" | "wav" | "m4a" | "aac" | "alac" | "ogg" | "aiff" | "aif"
    )
}

pub fn decode_to_pcm(
    file_path: &str,
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let file = File::open(file_path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(file_path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;

    let mut format = probed.format;

    let track = format.default_track().ok_or("no default track")?;

    let codec_params = track.codec_params.clone();
    let track_id = track.id;
    let source_rate = codec_params.sample_rate.unwrap_or(44100);
    let source_channels = codec_params.channels.map(|c| c.count() as u32).unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    // Seek if requested
    if seek_s > 0.0 {
        use symphonia::core::formats::SeekMode;
        use symphonia::core::formats::SeekTo;
        use symphonia::core::units::Time;
        let _ = format.seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time: Time::from(seek_s),
                track_id: Some(track_id),
            },
        );
    }

    let mut all_samples: Vec<i16> = Vec::new();
    let max_samples = if max_duration_s > 0.0 {
        (max_duration_s * source_rate as f64 * source_channels as f64) as usize
    } else {
        usize::MAX
    };

    loop {
        if all_samples.len() >= max_samples {
            break;
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let spec = *decoded.spec();
        let num_frames = decoded.frames();

        let mut sample_buf = SampleBuffer::<i16>::new(num_frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);

        all_samples.extend_from_slice(sample_buf.samples());
    }

    if all_samples.len() > max_samples {
        all_samples.truncate(max_samples);
    }

    let out_rate = target_sample_rate.unwrap_or(source_rate);
    let out_channels = target_channels.unwrap_or(source_channels);
    let total_frames = all_samples.len() as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = file_path,
        samples = all_samples.len(),
        rate = source_rate,
        channels = source_channels,
        duration_s,
        "decoded_symphonia"
    );

    Ok(DecodedAudio {
        samples: all_samples,
        sample_rate: out_rate,
        channels: out_channels,
        duration_s,
    })
}
