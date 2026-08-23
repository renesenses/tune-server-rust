//! Offline rubato (sinc) resampling shared by the audio outputs and the file
//! converter.
//!
//! Extracted from `outputs/local.rs` so it is usable without the
//! `local-audio` feature — the converter (#1525) now resamples natively
//! instead of shelling out to an external ffmpeg.

use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, calculate_cutoff,
};
use tracing::{info, warn};

use super::simple_resample;

/// Resample a complete buffer of interleaved f32 samples using rubato sinc.
///
/// Used for compressed streams where all decoded data is available at once.
/// Creates and consumes a temporary resampler internally.
///
/// The output carries the sinc filter's group delay at the head and its
/// drained tail at the end (~2×delay extra frames in total) — harmless for
/// streaming, where the surrounding pipeline is already latency-tolerant.
/// Offline callers that need sample-exact output use
/// [`rubato_resample_batch_exact`].
pub fn rubato_resample_batch(samples: &[f32], from_sr: u32, to_sr: u32, channels: u16) -> Vec<f32> {
    rubato_batch_inner(samples, from_sr, to_sr, channels, false)
}

/// Like [`rubato_resample_batch`] but trims the resampler's group delay from
/// the head and truncates to the exact expected length
/// (`round(in_frames × to_sr / from_sr)`), so the signal keeps its timing.
/// This is what the file converter uses (#1525): a converted album must not
/// grow leading/trailing silence — it would break gapless playback.
pub fn rubato_resample_batch_exact(
    samples: &[f32],
    from_sr: u32,
    to_sr: u32,
    channels: u16,
) -> Vec<f32> {
    rubato_batch_inner(samples, from_sr, to_sr, channels, true)
}

fn rubato_batch_inner(
    samples: &[f32],
    from_sr: u32,
    to_sr: u32,
    channels: u16,
    exact: bool,
) -> Vec<f32> {
    if from_sr == to_sr || samples.is_empty() {
        return samples.to_vec();
    }
    let ch = channels as usize;
    if ch == 0 {
        return Vec::new();
    }

    let ratio = to_sr as f64 / from_sr as f64;
    // Adaptive resampler params based on conversion ratio:
    //   ratio ≤ 2.0 (e.g. 96kHz→48kHz): quality params, plenty of CPU budget
    //   ratio > 2.0 (e.g. 176.4kHz→48kHz, 192kHz→48kHz): lighter params
    //     to avoid real-time stuttering on Windows (still ~90dB SNR)
    let inv_ratio = 1.0 / ratio; // > 1.0 when downsampling
    let (sinc_len, oversampling_factor) = if inv_ratio > 2.0 {
        (32_usize, 64_usize) // lighter: 176.4/192kHz → 48kHz
    } else {
        (64_usize, 128_usize) // standard: 96kHz → 48kHz
    };
    let window = WindowFunction::BlackmanHarris2;
    let f_cutoff = calculate_cutoff(sinc_len, window);
    let params = SincInterpolationParameters {
        sinc_len,
        f_cutoff,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor,
        window,
    };
    let mut resampler =
        match Async::<f32>::new_sinc(ratio, 1.1, &params, 1024, ch, FixedAsync::Input) {
            Ok(r) => Some(r),
            Err(e) => {
                warn!(error = %e, "rubato_batch_resampler_creation_failed_using_linear");
                return simple_resample(samples, from_sr, to_sr, channels);
            }
        };
    let delay_frames = resampler
        .as_ref()
        .map(|r| r.output_delay())
        .unwrap_or_default();

    // Resample using the chunk helper, then flush
    let mut batch_leftover: Vec<f32> = Vec::new();
    let mut out = rubato_resample_chunk(
        &mut resampler,
        samples,
        channels,
        false,
        &mut batch_leftover,
    );
    let flushed = rubato_resample_chunk(&mut resampler, &[], channels, true, &mut batch_leftover);
    out.extend_from_slice(&flushed);

    if exact {
        // Drop the group delay at the head, keep exactly the expected frames.
        let expected = ((samples.len() / ch) as f64 * ratio).round() as usize;
        let skip = delay_frames.min(out.len() / ch) * ch;
        out.drain(..skip);
        out.truncate(expected * ch);
    }

    info!(
        from_sr,
        to_sr,
        in_samples = samples.len(),
        out_samples = out.len(),
        exact,
        "rubato_batch_resample_complete"
    );

    out
}

/// Resample a chunk of interleaved f32 samples using rubato's sinc resampler.
///
/// The resampler is created once per track and reused across chunks.
/// `samples` is interleaved f32, `channels` is the channel count *after*
/// any channel adaptation (i.e. the output channel count).
///
/// When `flush` is true, feeds silence into the resampler to drain its
/// internal buffers at end-of-stream. `samples` should be empty in that case.
pub fn rubato_resample_chunk(
    resampler: &mut Option<Async<f32>>,
    samples: &[f32],
    channels: u16,
    flush: bool,
    resample_leftover: &mut Vec<f32>,
) -> Vec<f32> {
    use rubato::audioadapter_buffers::direct::InterleavedSlice;
    use rubato::audioadapter_buffers::owned::InterleavedOwned;

    let Some(resampler) = resampler.as_mut() else {
        // No resampler available — pass through unchanged
        return samples.to_vec();
    };

    let ch = channels as usize;
    if ch == 0 {
        return Vec::new();
    }

    // Combine leftover from previous call with new samples.
    // This avoids using rubato's partial_len during continuous streaming,
    // which pads the remainder with silence and corrupts subsequent output
    // (perceived as white noise on 24-bit audio where frame counts rarely
    // align to the resampler's block size).
    let combined: Vec<f32>;
    let input_ref: &[f32] = if flush {
        // When flushing, drain leftover first, then feed silence
        if !resample_leftover.is_empty() {
            combined = resample_leftover.drain(..).collect();
            &combined
        } else {
            &[]
        }
    } else {
        if resample_leftover.is_empty() {
            // Fast path: no leftover, use new samples directly
            let usable = (samples.len() / ch) * ch;
            &samples[..usable]
        } else {
            // Prepend leftover from previous call
            combined = resample_leftover
                .drain(..)
                .chain(samples.iter().copied())
                .collect();
            let usable = (combined.len() / ch) * ch;
            // Any sub-frame remainder goes back to leftover (shouldn't happen
            // since both leftover and samples are frame-aligned, but be safe)
            if usable < combined.len() {
                resample_leftover.extend_from_slice(&combined[usable..]);
            }
            &combined[..usable]
        }
    };

    let actual_in_frames = input_ref.len() / ch;

    // Process only complete resampler blocks (input_frames_next() frames each).
    // Carry over any remaining frames to the next call instead of using
    // partial_len, which pads with silence and introduces artifacts.
    let mut all_output = Vec::new();
    let mut offset = 0;

    while offset < actual_in_frames {
        let chunk_needed = resampler.input_frames_next();
        let chunk_available = actual_in_frames - offset;

        if chunk_available < chunk_needed {
            if flush {
                // End of track: process remaining frames with silence padding
                let chunk_slice = &input_ref[offset * ch..actual_in_frames * ch];
                let input_adapter = match InterleavedSlice::new(chunk_slice, ch, chunk_available) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(error = %e, "rubato_input_adapter_error_flush");
                        break;
                    }
                };
                let out_frames = resampler.output_frames_next();
                let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);
                let indexing = rubato::Indexing {
                    input_offset: 0,
                    output_offset: 0,
                    partial_len: Some(chunk_available),
                    active_channels_mask: None,
                };
                match resampler.process_into_buffer(
                    &input_adapter,
                    &mut output_buf,
                    Some(&indexing),
                ) {
                    Ok((_nbr_in, nbr_out)) => {
                        let out_data = output_buf.take_data();
                        all_output.extend_from_slice(&out_data[..nbr_out * ch]);
                    }
                    Err(e) => {
                        warn!(error = %e, "rubato_process_error_flush");
                    }
                }
                offset = actual_in_frames;
            } else {
                // Continuous streaming: save remainder for next call
                resample_leftover.extend_from_slice(&input_ref[offset * ch..actual_in_frames * ch]);
                break;
            }
        } else {
            // Full block available — process without partial_len
            let chunk_slice = &input_ref[offset * ch..(offset + chunk_needed) * ch];
            let input_adapter = match InterleavedSlice::new(chunk_slice, ch, chunk_needed) {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "rubato_input_adapter_error");
                    break;
                }
            };

            let out_frames = resampler.output_frames_next();
            let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);

            match resampler.process_into_buffer(&input_adapter, &mut output_buf, None) {
                Ok((_nbr_in, nbr_out)) => {
                    let out_data = output_buf.take_data();
                    all_output.extend_from_slice(&out_data[..nbr_out * ch]);
                }
                Err(e) => {
                    warn!(error = %e, "rubato_process_error");
                    break;
                }
            }

            offset += chunk_needed;
        }
    }

    // If flushing and we processed all leftover above, now feed a block of
    // pure silence to drain the resampler's internal delay line.
    if flush && offset >= actual_in_frames {
        let silence_frames = resampler.input_frames_next();
        let silence = vec![0.0f32; silence_frames * ch];
        let input_adapter = match InterleavedSlice::new(&silence, ch, silence_frames) {
            Ok(a) => a,
            Err(_) => return all_output,
        };
        let out_frames = resampler.output_frames_next();
        let mut output_buf = InterleavedOwned::<f32>::new(0.0f32, ch, out_frames);
        let indexing = rubato::Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(0),
            active_channels_mask: None,
        };
        if let Ok((_nbr_in, nbr_out)) =
            resampler.process_into_buffer(&input_adapter, &mut output_buf, Some(&indexing))
        {
            let out_data = output_buf.take_data();
            all_output.extend_from_slice(&out_data[..nbr_out * ch]);
        }
    }

    all_output
}

/// Resample interleaved i32 PCM (at `bit_depth`) from `from_sr` to `to_sr`,
/// returning i32 samples at the same bit depth.
///
/// Converts to normalized f32 for the sinc pass, then back with clamping.
/// This is the converter's offline entry point — quality over speed.
pub fn resample_i32(
    samples: &[i32],
    bit_depth: u16,
    channels: u16,
    from_sr: u32,
    to_sr: u32,
) -> Vec<i32> {
    if from_sr == to_sr || samples.is_empty() {
        return samples.to_vec();
    }
    let full_scale = (1i64 << (bit_depth.clamp(8, 32) - 1)) as f32;
    let as_f32: Vec<f32> = samples.iter().map(|&s| s as f32 / full_scale).collect();
    let resampled = rubato_resample_batch_exact(&as_f32, from_sr, to_sr, channels);
    let max = full_scale - 1.0;
    resampled
        .iter()
        .map(|&v| (v * full_scale).clamp(-full_scale, max) as i32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 s of a 440 Hz sine, stereo interleaved, at `sr`.
    fn sine_stereo(sr: u32) -> Vec<f32> {
        let mut out = Vec::with_capacity(sr as usize * 2);
        for n in 0..sr {
            let v = (2.0 * std::f32::consts::PI * 440.0 * n as f32 / sr as f32).sin() * 0.5;
            out.push(v);
            out.push(v);
        }
        out
    }

    #[test]
    fn batch_exact_length_matches_ratio() {
        // 44.1 kHz → 48 kHz: one second in, EXACTLY one second out. The
        // non-exact variant keeps the sinc delay + drained tail (streaming
        // contract); the converter must not — extra frames would break
        // gapless playback of a converted album.
        let input = sine_stereo(44100);
        let out = rubato_resample_batch_exact(&input, 44100, 48000, 2);
        assert_eq!(out.len() / 2, 48000, "exact variant must hit the ratio");

        let streaming = rubato_resample_batch(&input, 44100, 48000, 2);
        assert!(
            streaming.len() >= out.len(),
            "streaming variant carries the delay/tail"
        );
    }

    #[test]
    fn batch_exact_does_not_shift_the_signal() {
        // The trimmed head must be the filter delay, not the first samples of
        // the signal: a full-scale click at t=0 must still be near t=0 after
        // resampling, not `delay` frames later.
        let mut input = vec![0.0f32; 44100 * 2];
        input[0] = 1.0;
        input[1] = 1.0;
        let out = rubato_resample_batch_exact(&input, 44100, 48000, 2);
        let peak_frame = out
            .chunks(2)
            .enumerate()
            .max_by(|a, b| a.1[0].abs().partial_cmp(&b.1[0].abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert!(
            peak_frame < 16,
            "click drifted to frame {peak_frame}: delay not trimmed correctly"
        );
    }

    #[test]
    fn batch_resample_preserves_energy() {
        // A sine's RMS must survive the resampler: silence out would be the
        // converter's old ffmpeg failure mode, loud this time.
        let input = sine_stereo(96000);
        let out = rubato_resample_batch(&input, 96000, 44100, 2);
        let rms = |s: &[f32]| (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
        let (in_rms, out_rms) = (rms(&input), rms(&out));
        assert!(
            (in_rms - out_rms).abs() < 0.02,
            "RMS drifted: in={in_rms} out={out_rms}"
        );
    }

    #[test]
    fn resample_i32_round_trips_bit_depth() {
        // 16-bit full-scale-ish sine: output stays 16-bit-ranged, same channels.
        let sr_in = 44100u32;
        let samples: Vec<i32> = (0..sr_in)
            .flat_map(|n| {
                let v = ((2.0 * std::f64::consts::PI * 440.0 * n as f64 / sr_in as f64).sin()
                    * 16000.0) as i32;
                [v, v]
            })
            .collect();
        let out = resample_i32(&samples, 16, 2, sr_in, 48000);
        assert!(!out.is_empty());
        assert!(out.iter().all(|&v| (-32768..=32767).contains(&v)));
        let peak = out.iter().map(|v| v.abs()).max().unwrap();
        assert!(peak > 8000, "signal lost in resampling: peak={peak}");
    }

    #[test]
    fn same_rate_is_identity() {
        let input = sine_stereo(48000);
        let out = rubato_resample_batch(&input, 48000, 48000, 2);
        assert_eq!(input, out);
    }

    /// Le contrat de `rubato_resample_chunk` : quand `flush` vaut `true`,
    /// l'argument `samples` n'est JAMAIS lu.
    ///
    /// Ce n'est pas un detail d'implementation : la PR #2290 passait la queue
    /// du convolveur directement en `flush = true`, et cette queue etait
    /// silencieusement jetee. Le correctif cense restituer la fin de la
    /// convolution ne produisait donc rien des que le chemin reechantillonnait
    /// (#2295, JP Robbe).
    ///
    /// On compare la sequence de reference — bloc normal, puis vidage sur une
    /// tranche vide — au raccourci fautif, avec et sans leftover, sur un
    /// rapport non entier (44,1 → 48 kHz).
    fn resampleur_44_vers_48(ch: usize) -> Option<Async<f32>> {
        use rubato::{
            FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction,
            calculate_cutoff,
        };
        let sinc_len = 64;
        let window = WindowFunction::BlackmanHarris2;
        let params = SincInterpolationParameters {
            sinc_len,
            f_cutoff: calculate_cutoff(sinc_len, window),
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window,
        };
        Some(
            Async::<f32>::new_sinc(48000.0 / 44100.0, 1.1, &params, 1024, ch, FixedAsync::Input)
                .unwrap(),
        )
    }

    fn energie(v: &[f32]) -> f64 {
        v.iter().map(|s| (*s as f64) * (*s as f64)).sum()
    }

    #[test]
    fn le_vidage_du_resampleur_ignore_ses_echantillons() {
        const CH: usize = 2;
        // Une queue FRANCHE : 900 trames a pleine amplitude. Si elle traverse,
        // son energie est visible ; si elle est jetee, il ne reste rien.
        let queue: Vec<f32> = (0..900 * CH)
            .map(|i| (i as f32 * 0.01).sin() * 0.5)
            .collect();

        for avec_leftover in [false, true] {
            let mut ref_r = resampleur_44_vers_48(CH);
            let mut ref_lo: Vec<f32> = Vec::new();
            let mut fautif_r = resampleur_44_vers_48(CH);
            let mut fautif_lo: Vec<f32> = Vec::new();

            if avec_leftover {
                // 300 trames < bloc de 1024 : elles partent entierement en
                // leftover, des deux cotes, a l'identique. Amplitude minuscule
                // pour qu'elles ne puissent pas masquer la queue.
                let amorce: Vec<f32> = (0..300 * CH)
                    .map(|i| (i as f32 * 0.02).sin() * 1e-4)
                    .collect();
                rubato_resample_chunk(&mut ref_r, &amorce, CH as u16, false, &mut ref_lo);
                rubato_resample_chunk(&mut fautif_r, &amorce, CH as u16, false, &mut fautif_lo);
                assert_eq!(ref_lo.len(), fautif_lo.len(), "amorce dissymetrique");
                assert!(!ref_lo.is_empty(), "l'amorce devait laisser un leftover");
            }

            // Reference : la queue est un bloc audio comme un autre, PUIS on
            // vide le retard propre du resampleur sur une tranche vide.
            let mut reference =
                rubato_resample_chunk(&mut ref_r, &queue, CH as u16, false, &mut ref_lo);
            reference.extend_from_slice(&rubato_resample_chunk(
                &mut ref_r,
                &[],
                CH as u16,
                true,
                &mut ref_lo,
            ));

            // Chemin #2290 : la queue passee directement avec flush = true.
            let fautif =
                rubato_resample_chunk(&mut fautif_r, &queue, CH as u16, true, &mut fautif_lo);

            assert!(
                energie(&reference) > energie(&fautif) * 100.0,
                "avec_leftover={avec_leftover} : la queue du convolveur doit \
                 atteindre la sortie. Reference {:.6e} contre {:.6e} pour le \
                 chemin qui la passe en flush — elle est jetee (#2295)",
                energie(&reference),
                energie(&fautif)
            );
            for s in reference.iter() {
                assert!(s.is_finite(), "sortie non finie");
            }
        }
    }
}
