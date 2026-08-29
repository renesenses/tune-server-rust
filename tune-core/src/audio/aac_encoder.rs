//! Native AAC-LC file encoder via the OS encoder (#1527).
//!
//! macOS: direct FFI to the AudioToolbox framework (`AudioConverterNew` +
//! `AudioConverterFillComplexBuffer`) — Apple's own AAC encoder, the same
//! one iTunes uses, licence question settled by the OS. No new crate.
//!
//! Other platforms return a clear error and the converter falls back to
//! the bundled ffmpeg (#1524). Windows (Media Foundation) is the next
//! step of #1527 — it needs validation on a real Windows machine.

/// AAC-LC frames per packet.
pub const FRAMES_PER_PACKET: u32 = 1024;

/// MPEG-4 sampling frequency index (ISO 14496-3) — also the set of rates
/// the encoder accepts; callers resample to 48 kHz for anything else.
pub fn freq_index(sample_rate: u32) -> Option<u8> {
    Some(match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => return None,
    })
}

/// Build the `esds` box for AAC-LC: ES / DecoderConfig / DecSpecificInfo
/// (the 2-byte AudioSpecificConfig) / SLConfig descriptors.
/// (macOS only: Media Foundation writes its own container.)
#[cfg(target_os = "macos")]
fn esds_box(sample_rate: u32, channels: u16, avg_bitrate: u32) -> Vec<u8> {
    let fi = freq_index(sample_rate).expect("caller resamples to a standard rate");
    // AudioSpecificConfig: 5 bits object type (2 = LC), 4 bits freq index,
    // 4 bits channel config, padding.
    let asc = [
        (2u8 << 3) | (fi >> 1),
        ((fi & 1) << 7) | ((channels as u8) << 3),
    ];

    fn descr(tag: u8, payload: &[u8]) -> Vec<u8> {
        // Expandable size, single byte is enough for our tiny descriptors.
        let mut d = vec![tag, payload.len() as u8];
        d.extend_from_slice(payload);
        d
    }

    let dec_specific = descr(0x05, &asc);
    let mut dec_config = Vec::new();
    dec_config.push(0x40); // objectTypeIndication: MPEG-4 audio
    dec_config.push(0x15); // streamType audio (0x05 << 2) | 0x01
    dec_config.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
    dec_config.extend_from_slice(&(avg_bitrate.saturating_mul(2)).to_be_bytes()); // max
    dec_config.extend_from_slice(&avg_bitrate.to_be_bytes()); // avg
    dec_config.extend_from_slice(&dec_specific);
    let dec_config = descr(0x04, &dec_config);

    let sl_config = descr(0x06, &[0x02]);

    let mut es = Vec::new();
    es.extend_from_slice(&0u16.to_be_bytes()); // ES_ID
    es.push(0); // flags
    es.extend_from_slice(&dec_config);
    es.extend_from_slice(&sl_config);
    let es = descr(0x03, &es);

    super::m4a::full_box(b"esds", 0, 0, &es)
}

/// Encode interleaved i16 samples at a standard AAC rate into a complete
/// `.m4a` byte stream. `bitrate_bps` is the target bitrate.
#[cfg(target_os = "macos")]
pub fn encode_aac_m4a(
    pcm: &[i16],
    channels: u16,
    sample_rate: u32,
    bitrate_bps: u32,
) -> Result<Vec<u8>, String> {
    use audiotoolbox_ffi::*;

    if !(1..=2).contains(&channels) {
        return Err(format!(
            "aac: {channels} canaux non pris en charge (mono/stéréo)"
        ));
    }
    if freq_index(sample_rate).is_none() {
        return Err(format!(
            "aac: fréquence {sample_rate} non standard (rééchantillonner)"
        ));
    }

    let ch = channels as u32;
    let input = AudioStreamBasicDescription {
        sample_rate: sample_rate as f64,
        format_id: fourcc(b"lpcm"),
        format_flags: K_AUDIO_FLAG_SIGNED_INT | K_AUDIO_FLAG_PACKED,
        bytes_per_packet: 2 * ch,
        frames_per_packet: 1,
        bytes_per_frame: 2 * ch,
        channels_per_frame: ch,
        bits_per_channel: 16,
        reserved: 0,
    };
    let output = AudioStreamBasicDescription {
        sample_rate: sample_rate as f64,
        format_id: fourcc(b"aac "),
        format_flags: 0,
        bytes_per_packet: 0,
        frames_per_packet: FRAMES_PER_PACKET,
        bytes_per_frame: 0,
        channels_per_frame: ch,
        bits_per_channel: 0,
        reserved: 0,
    };

    let mut conv: AudioConverterRef = std::ptr::null_mut();
    let st = unsafe { AudioConverterNew(&input, &output, &mut conv) };
    if st != 0 {
        return Err(format!("aac: AudioConverterNew a rendu {st}"));
    }
    // RAII: release the converter on every exit path.
    struct Conv(AudioConverterRef);
    impl Drop for Conv {
        fn drop(&mut self) {
            unsafe { AudioConverterDispose(self.0) };
        }
    }
    let conv = Conv(conv);

    let st = unsafe {
        AudioConverterSetProperty(
            conv.0,
            fourcc(b"brat"), // kAudioConverterEncodeBitRate
            4,
            &bitrate_bps as *const u32 as *const std::ffi::c_void,
        )
    };
    if st != 0 {
        return Err(format!("aac: réglage du débit refusé ({st})"));
    }

    // Pull loop: the callback feeds PCM, we collect one AAC packet per call.
    struct FeedState<'a> {
        pcm: &'a [i16],
        offset: usize, // in frames
        channels: usize,
    }
    unsafe extern "C-unwind" fn feed(
        _conv: AudioConverterRef,
        io_num_packets: *mut u32,
        io_data: *mut AudioBufferList,
        _out_desc: *mut *mut AudioStreamPacketDescription,
        user: *mut std::ffi::c_void,
    ) -> i32 {
        let state = unsafe { &mut *(user as *mut FeedState) };
        let total_frames = state.pcm.len() / state.channels;
        let remaining = total_frames - state.offset;
        if remaining == 0 {
            unsafe { *io_num_packets = 0 };
            // Non-zero, non-system status: tells the converter "no more
            // input"; FillComplexBuffer surfaces it once output is drained.
            return END_OF_INPUT;
        }
        let take = remaining.min(FRAMES_PER_PACKET as usize * 4);
        let start = state.offset * state.channels;
        let slice = &state.pcm[start..start + take * state.channels];
        unsafe {
            (*io_data).number_buffers = 1;
            (*io_data).buffers[0] = AudioBuffer {
                number_channels: state.channels as u32,
                data_byte_size: (slice.len() * 2) as u32,
                data: slice.as_ptr() as *mut std::ffi::c_void,
            };
            *io_num_packets = take as u32;
        }
        state.offset += take;
        0
    }

    let mut state = FeedState {
        pcm,
        offset: 0,
        channels: channels as usize,
    };
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut out_buf = vec![0u8; 32 * 1024];
    loop {
        let mut out_packets: u32 = 1;
        let mut desc = AudioStreamPacketDescription {
            start_offset: 0,
            variable_frames: 0,
            data_byte_size: 0,
        };
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: ch,
                data_byte_size: out_buf.len() as u32,
                data: out_buf.as_mut_ptr() as *mut std::ffi::c_void,
            }],
        };
        let st = unsafe {
            AudioConverterFillComplexBuffer(
                conv.0,
                feed,
                &mut state as *mut FeedState as *mut std::ffi::c_void,
                &mut out_packets,
                &mut list,
                &mut desc,
            )
        };
        if out_packets > 0 && desc.data_byte_size > 0 {
            packets.push(out_buf[..desc.data_byte_size as usize].to_vec());
        }
        if st == END_OF_INPUT && out_packets == 0 {
            break; // drained
        }
        if st != 0 && st != END_OF_INPUT {
            return Err(format!("aac: encodage a rendu {st}"));
        }
        if st == 0 && out_packets == 0 {
            break; // defensive: nothing produced, nothing pending
        }
    }
    if packets.is_empty() {
        return Err("aac: aucun paquet produit".into());
    }

    let total_frames = packets.len() as u64 * FRAMES_PER_PACKET as u64;
    Ok(super::m4a::mux(&super::m4a::M4aTrack {
        sample_entry_kind: *b"mp4a",
        codec_box: esds_box(sample_rate, channels, bitrate_bps),
        packets: &packets,
        frames_per_packet: FRAMES_PER_PACKET,
        total_frames,
        sample_rate,
        channels,
        bit_depth: 16,
    }))
}

/// Windows: Media Foundation `IMFSinkWriter` — the OS encoder writes the
/// complete `.m4a` itself (no hand muxing). The exact call sequence was
/// validated on a real Windows 11 machine (.42) with a standalone probe:
/// the produced file decodes through the project's symphonia chain with
/// the right rate/channels and the source RMS (2026-08-13).
#[cfg(windows)]
pub fn encode_aac_m4a(
    pcm: &[i16],
    channels: u16,
    sample_rate: u32,
    bitrate_bps: u32,
) -> Result<Vec<u8>, String> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::HSTRING;

    if !(1..=2).contains(&channels) {
        return Err(format!(
            "aac: {channels} canaux non pris en charge (mono/stéréo)"
        ));
    }
    if !rate_supported(sample_rate) {
        return Err(format!(
            "aac: fréquence {sample_rate} non prise en charge (44100/48000)"
        ));
    }
    let ch = channels as u32;

    // MF's AAC encoder only accepts 12000/16000/20000/24000 bytes/s
    // (96/128/160/192 kb/s) — snap the request to the nearest.
    let target = bitrate_bps / 8;
    let bytes_per_sec = [12000u32, 16000, 20000, 24000]
        .into_iter()
        .min_by_key(|&v| v.abs_diff(target))
        .unwrap();

    // The sink writer writes a file; produce it in a temp path and return
    // the bytes to keep the same contract as the AudioToolbox path.
    let tmp = std::env::temp_dir().join(format!(
        "tune-aac-{}-{}.m4a",
        std::process::id(),
        pcm.as_ptr() as usize
    ));
    let tmp_str = tmp.to_string_lossy().to_string();

    let result: windows::core::Result<()> = (|| unsafe {
        // Per-thread COM init: the converter encodes from blocking-pool
        // threads. S_FALSE (already initialised) is fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // MF_VERSION = (MF_SDK_VERSION << 16) | MF_API_VERSION
        MFStartup(0x0002_0070, MFSTARTUP_FULL)?;

        let writer: IMFSinkWriter =
            MFCreateSinkWriterFromURL(&HSTRING::from(tmp_str.as_str()), None, None)?;

        let out_type: IMFMediaType = MFCreateMediaType()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        out_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        out_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch)?;
        out_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        out_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_sec)?;
        // AAC-LC, level indication the MF encoder expects.
        out_type.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        let stream_index = writer.AddStream(&out_type)?;

        let in_type: IMFMediaType = MFCreateMediaType()?;
        in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        in_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, ch)?;
        in_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        in_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2 * ch)?;
        in_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * 2 * ch)?;
        writer.SetInputMediaType(stream_index, &in_type, None)?;

        writer.BeginWriting()?;

        // Feed in ~100 ms slices, timestamps in 100 ns units.
        let bytes: &[u8] = std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2);
        let chunk_bytes = (sample_rate / 10) as usize * 2 * ch as usize;
        let mut written_frames: u64 = 0;
        for chunk in bytes.chunks(chunk_bytes) {
            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(chunk.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(chunk.as_ptr(), data, chunk.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(chunk.len() as u32)?;

            let sample: IMFSample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let frames = (chunk.len() / (2 * ch as usize)) as u64;
            sample.SetSampleTime((written_frames * 10_000_000 / sample_rate as u64) as i64)?;
            sample.SetSampleDuration((frames * 10_000_000 / sample_rate as u64) as i64)?;
            writer.WriteSample(stream_index, &sample)?;
            written_frames += frames;
        }

        writer.Finalize()?;
        MFShutdown()?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            let bytes = std::fs::read(&tmp)
                .map_err(|e| format!("aac: relecture du fichier temporaire: {e}"))?;
            let _ = std::fs::remove_file(&tmp);
            Ok(bytes)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("aac: Media Foundation a rendu {e}"))
        }
    }
}

/// Other platforms: no system encoder — the converter falls back to the
/// bundled ffmpeg (#1524).
#[cfg(not(any(target_os = "macos", windows)))]
pub fn encode_aac_m4a(
    _pcm: &[i16],
    _channels: u16,
    _sample_rate: u32,
    _bitrate_bps: u32,
) -> Result<Vec<u8>, String> {
    Err("aac: encodeur système indisponible sur cette plateforme".into())
}

/// Whether THIS build carries a native AAC encoder.
pub fn native_available() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// Whether the platform encoder accepts this input rate directly — the
/// converter resamples to 48 kHz otherwise. AudioToolbox takes every
/// standard MPEG-4 rate; Media Foundation's AAC encoder only 44.1/48 kHz.
pub fn rate_supported(sample_rate: u32) -> bool {
    if cfg!(windows) {
        matches!(sample_rate, 44100 | 48000)
    } else {
        freq_index(sample_rate).is_some()
    }
}

#[cfg(target_os = "macos")]
mod audiotoolbox_ffi {
    //! Just enough of AudioToolbox for offline AAC encoding.
    use std::ffi::c_void;

    pub const K_AUDIO_FLAG_SIGNED_INT: u32 = 0x4; // kAudioFormatFlagIsSignedInteger
    pub const K_AUDIO_FLAG_PACKED: u32 = 0x8; // kAudioFormatFlagIsPacked
    /// Our sentinel "no more input" status returned by the feed callback.
    pub const END_OF_INPUT: i32 = 0x74756E65; // 'tune'

    pub fn fourcc(b: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*b)
    }

    pub type AudioConverterRef = *mut c_void;

    #[repr(C)]
    pub struct AudioStreamBasicDescription {
        pub sample_rate: f64,
        pub format_id: u32,
        pub format_flags: u32,
        pub bytes_per_packet: u32,
        pub frames_per_packet: u32,
        pub bytes_per_frame: u32,
        pub channels_per_frame: u32,
        pub bits_per_channel: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    pub struct AudioBuffer {
        pub number_channels: u32,
        pub data_byte_size: u32,
        pub data: *mut c_void,
    }

    #[repr(C)]
    pub struct AudioBufferList {
        pub number_buffers: u32,
        pub buffers: [AudioBuffer; 1],
    }

    #[repr(C)]
    pub struct AudioStreamPacketDescription {
        pub start_offset: i64,
        pub variable_frames: u32,
        pub data_byte_size: u32,
    }

    pub type ComplexInputProc = unsafe extern "C-unwind" fn(
        AudioConverterRef,
        *mut u32,
        *mut AudioBufferList,
        *mut *mut AudioStreamPacketDescription,
        *mut c_void,
    ) -> i32;

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C-unwind" {
        pub fn AudioConverterNew(
            in_source: *const AudioStreamBasicDescription,
            in_dest: *const AudioStreamBasicDescription,
            out_converter: *mut AudioConverterRef,
        ) -> i32;
        pub fn AudioConverterDispose(conv: AudioConverterRef) -> i32;
        pub fn AudioConverterSetProperty(
            conv: AudioConverterRef,
            property_id: u32,
            size: u32,
            data: *const c_void,
        ) -> i32;
        pub fn AudioConverterFillComplexBuffer(
            conv: AudioConverterRef,
            proc_: ComplexInputProc,
            user_data: *mut c_void,
            io_output_data_packet_size: *mut u32,
            out_data: *mut AudioBufferList,
            out_packet_descriptions: *mut AudioStreamPacketDescription,
        ) -> i32;
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn sine_stereo_i16(sr: u32, seconds: f64) -> Vec<i16> {
        let frames = (sr as f64 * seconds) as usize;
        (0..frames)
            .flat_map(|n| {
                let v = ((2.0 * std::f64::consts::PI * 440.0 * n as f64 / sr as f64).sin()
                    * 16000.0) as i16;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn round_trip_through_project_decoder() {
        // AAC is lossy — no bit-exactness. The contract: the produced .m4a
        // is read back by the project's own decoder (symphonia isomp4+aac)
        // with the right rate/channels, a sane duration, and a live signal.
        let seconds = 1.0;
        let pcm = sine_stereo_i16(44100, seconds);
        let m4a = encode_aac_m4a(&pcm, 2, 44100, 192_000).expect("encode");

        let dir = std::env::temp_dir().join(format!("tune-aac-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.m4a");
        std::fs::write(&path, &m4a).unwrap();

        let decoded =
            crate::audio::decode::decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, f64::MAX)
                .expect("decode back");
        let _ = std::fs::remove_file(&path);

        assert_eq!(decoded.sample_rate, 44100);
        assert_eq!(decoded.channels, 2);
        // Priming/trailing padding allowed: within ~100 ms of the source.
        assert!(
            (decoded.duration_s - seconds).abs() < 0.1,
            "duration drifted: {}",
            decoded.duration_s
        );
        let rms = (decoded
            .samples_i32
            .iter()
            .map(|&s| (s as f64) * (s as f64))
            .sum::<f64>()
            / decoded.samples_i32.len() as f64)
            .sqrt();
        assert!(rms > 1000.0, "decoded signal is near-silent: rms={rms}");
    }

    #[test]
    fn rejects_nonstandard_rate_and_multichannel() {
        let pcm = sine_stereo_i16(44100, 0.1);
        assert!(encode_aac_m4a(&pcm, 6, 44100, 192_000).is_err());
        assert!(encode_aac_m4a(&pcm, 2, 44_099, 192_000).is_err());
    }
}
