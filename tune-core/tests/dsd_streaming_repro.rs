//! Reproducer for the "DSD64 DSF plays silent" report (Pierre Mack).
//!
//! The batch `decode_to_pcm` path is already known to produce FULL audio for
//! the offending file. This exercises the *streaming* path that LOCAL playback
//! actually uses — `decode_to_pcm_streaming_seeked(.., Some(176400), Some(2),
//! Some(32), ..)` — exactly as `orchestrator.rs` calls it, drains the streamed
//! bytes, and asserts the PCM is not pure silence.
//!
//! CI-safe: only runs when TUNE_DSD_REAL_FILE points at an existing file.
//!   TUNE_DSD_REAL_FILE="/path/to/file.dsf" cargo test -p tune-core \
//!     --test integration_contracts dsd_streaming_repro:: -- --nocapture

/// Drive the streaming decode exactly as playback does, at `out_bd` bits, and
/// return (nonzero_samples, total_samples, peak_dbfs). `out_bd` is 32 for LOCAL
/// output and 24 for the network progressive-WAV path.
async fn run_streaming(path: &str, out_bd: u16) -> (u64, u64, f64) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let data_ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let (levels_tx, _levels_rx) = tokio::sync::mpsc::unbounded_channel();

    // Drain concurrently — the decoder blocks on a bounded send.
    let consumer = tokio::spawn(async move {
        let mut buf = Vec::new();
        while let Some(chunk) = rx.recv().await {
            buf.extend_from_slice(&chunk);
        }
        buf
    });

    let dr = data_ready.clone();
    let p = path.to_string();
    let ret = tokio::task::spawn_blocking(move || {
        tune_core::audio::decode::decode_to_pcm_streaming_seeked(
            &p,
            Some(176_400),
            Some(2),
            Some(out_bd),
            tx,
            32_768,
            dr,
            levels_tx,
            0.0,
        )
    })
    .await
    .expect("decode task panicked");

    let bytes = consumer.await.expect("consumer panicked");
    ret.expect("streaming decode returned Err");
    assert!(bytes.len() > 44, "no data streamed ({} bytes)", bytes.len());

    let pcm = &bytes[44..]; // strip WAV header
    let bytes_per_sample = (out_bd / 8) as usize;
    let mut nonzero: u64 = 0;
    let mut peak: i64 = 0;
    // Full-scale magnitude for this bit depth (samples are left-justified within
    // their byte width), used to normalise the peak to dBFS comparably at any bd.
    let full_scale: i64 = 1i64 << (out_bd - 1);
    for s in pcm.chunks_exact(bytes_per_sample) {
        // Interpret the little-endian sample of `bytes_per_sample` bytes, sign-
        // extended, then scale to a common [-full_scale, full_scale) range.
        let v: i64 = match bytes_per_sample {
            3 => {
                let raw = (s[0] as i64) | ((s[1] as i64) << 8) | ((s[2] as i64) << 16);
                if raw & 0x80_0000 != 0 {
                    raw - 0x100_0000
                } else {
                    raw
                }
            }
            4 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as i64,
            _ => unreachable!(),
        };
        if v != 0 {
            nonzero += 1;
        }
        let a = v.abs();
        if a > peak {
            peak = a;
        }
    }
    let total = (pcm.len() / bytes_per_sample) as u64;
    let peak_dbfs = if peak > 0 {
        20.0 * (peak as f64 / full_scale as f64).log10()
    } else {
        f64::NEG_INFINITY
    };
    eprintln!(
        "STREAMING out_bd={out_bd} pcm_samples={total} nonzero={nonzero} peak={peak} peak_dbfs={peak_dbfs:.1}"
    );
    (nonzero, total, peak_dbfs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsd_streaming_local_and_network_paths_valid() {
    let path = match std::env::var("TUNE_DSD_REAL_FILE") {
        Ok(p) if std::path::Path::new(&p).exists() => p,
        _ => {
            eprintln!("skip: set TUNE_DSD_REAL_FILE to an existing .dsf/.dff");
            return;
        }
    };

    // LOCAL path (32-bit) and the new NETWORK progressive-WAV path (24-bit).
    let (nz32, _t32, peak32) = run_streaming(&path, 32).await;
    let (nz24, _t24, peak24) = run_streaming(&path, 24).await;

    assert!(nz32 > 0, "32-bit streaming decode is PURE SILENCE");
    assert!(nz24 > 0, "24-bit streaming decode is PURE SILENCE");

    // The known-good reference peak for this file is ~-23.9 dBFS. A 24-bit
    // packing misalignment (the white-noise class the local path warns about)
    // would smear energy toward full-scale, so the peaks must AGREE closely.
    // 3 dB tolerance is generous — a misalignment would blow well past it.
    assert!(
        (peak24 - peak32).abs() < 3.0,
        "24-bit peak {peak24:.1} dBFS diverges from 32-bit {peak32:.1} dBFS — \
         24-bit packing is misaligned (would be white noise on the renderer)"
    );
    assert!(
        peak24 < -3.0,
        "24-bit peak {peak24:.1} dBFS is near full-scale — looks like noise, not audio"
    );
}
