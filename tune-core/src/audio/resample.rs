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

/// Build the stateful sinc resampler used by progressive PCM pipelines.
///
/// Callers that promise an exact output rate must propagate this error: using
/// source-rate samples after announcing `to_sr` would corrupt speed and pitch.
pub fn new_streaming_resampler(
    from_sr: u32,
    to_sr: u32,
    channels: u16,
) -> Result<Async<f32>, String> {
    if from_sr == 0 || to_sr == 0 || channels == 0 {
        return Err(format!(
            "invalid resampler format: {from_sr} Hz -> {to_sr} Hz, {channels} channels"
        ));
    }
    let ratio = to_sr as f64 / from_sr as f64;
    let inv_ratio = 1.0 / ratio;
    // A fixed short kernel makes the transition band wider in output-Hz as
    // the decimation ratio grows. At the former 32/64 taps, 48 -> 44.1 kHz
    // was already down 10.4 dB at 18 kHz (#2711). Scale the kernel at the
    // supported rate boundaries so every path remains within 0.1 dB there.
    let (sinc_len, oversampling_factor) = if inv_ratio > 4.0 {
        (512_usize, 256_usize)
    } else if inv_ratio > 2.0 {
        (256_usize, 256_usize)
    } else {
        (128_usize, 256_usize)
    };
    let window = WindowFunction::BlackmanHarris2;
    let params = SincInterpolationParameters {
        sinc_len,
        f_cutoff: calculate_cutoff(sinc_len, window),
        interpolation: SincInterpolationType::Linear,
        oversampling_factor,
        window,
    };
    Async::<f32>::new_sinc(
        ratio,
        1.1,
        &params,
        1024,
        channels as usize,
        FixedAsync::Input,
    )
    .map_err(|e| format!("create sinc resampler {from_sr} -> {to_sr} Hz: {e}"))
}

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

/// Adaptation de cadence d'une piste dont TOUT le contenu est deja en memoire
/// (#2246) — flux compresse decode en bloc : FLAC, MP3, AAC, ALAC…
///
/// Ce que compense le retrait du delai de groupe
/// --------------------------------------------
/// Le delai de groupe n'est pas un alignement voulu entre deux signaux : c'est
/// la latence propre du filtre sinc. Les `output_delay()` premieres trames de
/// sortie sont la montee en regime du FIR, pas de la matiere musicale, et le
/// drainage de fin ajoute une queue de rembourrage symetrique. Les garder
/// decale l'instant zero de la piste et allonge sa duree de ~2×delay trames.
///
/// Pourquoi ici, et surtout POURQUOI PAS sur le chemin en flux
/// -----------------------------------------------------------
/// Un rééchantillonneur cree pour UNE piste entiere paie son delai UNE FOIS
/// PAR PISTE : le decalage se reinstalle a chaque frontiere, et la duree/
/// position calculees sur cette sortie allongee mentent d'autant.
///
/// Le chemin PCM en flux de `outputs/local.rs` est l'inverse exact : le
/// rééchantillonneur y est cree une fois pour la CHAINE et conserve d'une
/// piste a l'autre tant que la cadence source ne change pas (vrai gapless).
/// Il paie donc son delai une seule fois pour tout l'enchainement — une
/// latence constante, deja absorbee. Y retrancher le delai a chaque piste
/// **supprimerait de la matiere reelle** a chaque frontiere : ce serait
/// exactement le mauvais correctif. Cette fonction est reservee aux appelants
/// qui detiennent la piste complete et jettent leur rééchantillonneur avec.
///
/// C'est le meme contrat que `StreamingPcmAdapter` (`audio/decode.rs`) tient
/// deja pour les decodages progressifs, et que le convertisseur (#1525) tient
/// pour les fichiers.
pub fn rubato_resample_track(samples: &[f32], from_sr: u32, to_sr: u32, channels: u16) -> Vec<f32> {
    rubato_resample_batch_exact(samples, from_sr, to_sr, channels)
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
    let mut resampler = match new_streaming_resampler(from_sr, to_sr, channels) {
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

    fn signal(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|frame| {
                (0..channels).map(move |channel| {
                    let phase = frame as f32 * 0.037 + channel as f32 * 0.19;
                    phase.sin() * 0.5
                })
            })
            .collect()
    }

    fn sine_mono(sr: u32, frequency_hz: f64, amplitude: f64) -> Vec<f32> {
        (0..sr)
            .map(|frame| {
                (amplitude
                    * (2.0 * std::f64::consts::PI * frequency_hz * frame as f64 / sr as f64).sin())
                    as f32
            })
            .collect()
    }

    /// Amplitude of one known sinusoid, evaluated directly rather than through
    /// the spectrum display code. Keeping the measuring instrument independent
    /// from the production DSP prevents both sides from sharing the same bug.
    fn measured_tone_amplitude(samples: &[f32], sr: u32, frequency_hz: f64) -> f64 {
        let trim = (samples.len() / 10).min(4_800);
        let stable = &samples[trim..samples.len() - trim];
        let (sin_sum, cos_sum) = stable.iter().enumerate().fold(
            (0.0_f64, 0.0_f64),
            |(sin_sum, cos_sum), (frame, &sample)| {
                let phase = 2.0 * std::f64::consts::PI * frequency_hz * frame as f64 / sr as f64;
                (
                    sin_sum + sample as f64 * phase.sin(),
                    cos_sum + sample as f64 * phase.cos(),
                )
            },
        );
        2.0 * sin_sum.hypot(cos_sum) / stable.len() as f64
    }

    fn gain_db(measured: f64, reference: f64) -> f64 {
        20.0 * (measured / reference).log10()
    }

    /// Fit the fundamental in quadrature and report everything left over as
    /// distortion plus noise. This is deliberately a time-domain least-squares
    /// measurement, independent from both the resampler and the FFT display.
    fn thd_plus_noise_db(samples: &[f32], sr: u32, frequency_hz: f64) -> f64 {
        let trim = (samples.len() / 10).min(4_800);
        let stable = &samples[trim..samples.len() - trim];
        let (sin_sum, cos_sum) = stable.iter().enumerate().fold(
            (0.0_f64, 0.0_f64),
            |(sin_sum, cos_sum), (frame, &sample)| {
                let phase = 2.0 * std::f64::consts::PI * frequency_hz * frame as f64 / sr as f64;
                (
                    sin_sum + sample as f64 * phase.sin(),
                    cos_sum + sample as f64 * phase.cos(),
                )
            },
        );
        let sin_gain = 2.0 * sin_sum / stable.len() as f64;
        let cos_gain = 2.0 * cos_sum / stable.len() as f64;
        let fundamental_rms = sin_gain.hypot(cos_gain) / std::f64::consts::SQRT_2;
        let residual_rms = (stable
            .iter()
            .enumerate()
            .map(|(frame, &sample)| {
                let phase = 2.0 * std::f64::consts::PI * frequency_hz * frame as f64 / sr as f64;
                let fitted = sin_gain * phase.sin() + cos_gain * phase.cos();
                (sample as f64 - fitted).powi(2)
            })
            .sum::<f64>()
            / stable.len() as f64)
            .sqrt();
        20.0 * (residual_rms / fundamental_rms).log10()
    }

    fn resample_in_chunks(
        samples: &[f32],
        from_sr: u32,
        to_sr: u32,
        channels: u16,
        chunk_frames: &[usize],
    ) -> (Vec<f32>, Vec<f32>) {
        let ch = usize::from(channels);
        let mut resampler = Some(
            new_streaming_resampler(from_sr, to_sr, channels)
                .expect("la matrice emploie uniquement des formats valides"),
        );
        let mut leftover = Vec::new();
        let mut output = Vec::new();
        let mut frame = 0;
        let total_frames = samples.len() / ch;
        let mut chunk = 0;

        while frame < total_frames {
            let frames = chunk_frames[chunk % chunk_frames.len()].min(total_frames - frame);
            chunk += 1;
            let start = frame * ch;
            let end = (frame + frames) * ch;
            output.extend(rubato_resample_chunk(
                &mut resampler,
                &samples[start..end],
                channels,
                false,
                &mut leftover,
            ));
            frame += frames;
        }
        output.extend(rubato_resample_chunk(
            &mut resampler,
            &[],
            channels,
            true,
            &mut leftover,
        ));
        (output, leftover)
    }

    #[test]
    fn exact_frame_count_covers_common_ratios_channels_and_boundaries() {
        let ratios = [
            (44_100, 48_000),
            (48_000, 44_100),
            (48_000, 96_000),
            (96_000, 44_100),
            (192_000, 48_000),
        ];

        for (from_sr, to_sr) in ratios {
            for channels in [1_u16, 2] {
                for input_frames in [1_usize, 37, 1_023, 1_024, 1_025, 4_417] {
                    let input = signal(input_frames, usize::from(channels));
                    let output = rubato_resample_batch_exact(&input, from_sr, to_sr, channels);
                    let expected =
                        (input_frames as f64 * to_sr as f64 / from_sr as f64).round() as usize;

                    assert_eq!(
                        output.len(),
                        expected * usize::from(channels),
                        "{from_sr} -> {to_sr} Hz, {channels} canal(aux), {input_frames} trames"
                    );
                }
            }
        }
    }

    #[test]
    fn streaming_flush_is_invariant_to_chunk_boundaries() {
        for (from_sr, to_sr) in [(44_100, 48_000), (96_000, 44_100)] {
            for channels in [1_u16, 2] {
                let input = signal(5_137, usize::from(channels));
                let reference = rubato_resample_batch(&input, from_sr, to_sr, channels);
                let (chunked, leftover) = resample_in_chunks(
                    &input,
                    from_sr,
                    to_sr,
                    channels,
                    &[1, 17, 1_000, 3, 2_048, 511],
                );

                assert!(
                    leftover.is_empty(),
                    "le flush laisse des échantillons en attente pour {from_sr} -> {to_sr} Hz"
                );
                assert_eq!(
                    chunked, reference,
                    "le découpage change la sortie ou son nombre de trames pour \
                     {from_sr} -> {to_sr} Hz, {channels} canal(aux)"
                );
            }
        }
    }

    #[test]
    fn downsampling_stepped_sweep_preserves_passband_and_rejects_alias() {
        const AMPLITUDE: f64 = 0.5;

        for (from_sr, to_sr) in [
            (384_000, 48_000),
            (192_000, 48_000),
            (96_000, 48_000),
            (48_000, 44_100),
        ] {
            for frequency_hz in [
                50.0, 100.0, 250.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 15_000.0, 18_000.0,
            ] {
                let input = sine_mono(from_sr, frequency_hz, AMPLITUDE);
                let output = rubato_resample_batch_exact(&input, from_sr, to_sr, 1);
                let response_db = gain_db(
                    measured_tone_amplitude(&output, to_sr, frequency_hz),
                    AMPLITUDE,
                );
                assert!(
                    response_db.abs() < 0.1,
                    "la bande utile {from_sr} -> {to_sr} Hz derive de \
                     {response_db:.3} dB a {frequency_hz} Hz"
                );
            }
        }

        // 30 kHz cannot exist at 48 kHz. A converter which merely decimates
        // would fold it to |30 - 48| = 18 kHz at essentially full amplitude.
        for from_sr in [96_000, 192_000, 384_000] {
            let input = sine_mono(from_sr, 30_000.0, AMPLITUDE);
            let output = rubato_resample_batch_exact(&input, from_sr, 48_000, 1);
            let alias_db = gain_db(
                measured_tone_amplitude(&output, 48_000, 18_000.0),
                AMPLITUDE,
            );

            assert!(
                alias_db < -80.0,
                "un sinus 30 kHz a {from_sr} Hz se replie a 18 kHz a {alias_db:.1} dB"
            );
        }
    }

    #[test]
    fn upsampling_preserves_tone_and_rejects_spectral_image() {
        const FROM_SR: u32 = 44_100;
        const TO_SR: u32 = 96_000;
        const AMPLITUDE: f64 = 0.5;
        const TONE_HZ: f64 = 1_000.0;

        let input = sine_mono(FROM_SR, TONE_HZ, AMPLITUDE);
        let output = rubato_resample_batch_exact(&input, FROM_SR, TO_SR, 1);
        let tone_db = gain_db(measured_tone_amplitude(&output, TO_SR, TONE_HZ), AMPLITUDE);
        assert!(
            tone_db.abs() < 0.1,
            "le sinus utile derive de {tone_db:.3} dB pendant le suréchantillonnage"
        );

        // Repeating/interpolating samples without a proper reconstruction
        // filter would create the first image at FROM_SR - TONE_HZ.
        let image_hz = FROM_SR as f64 - TONE_HZ;
        let image_db = gain_db(measured_tone_amplitude(&output, TO_SR, image_hz), AMPLITUDE);
        assert!(
            image_db < -80.0,
            "le sinus 1 kHz produit une image a {image_hz} Hz a {image_db:.1} dB"
        );
    }

    #[test]
    fn resampling_keeps_sine_thd_plus_noise_below_minus_100_db() {
        const FREQUENCY_HZ: f64 = 1_000.0;
        const AMPLITUDE: f64 = 0.5;

        for (from_sr, to_sr) in [(44_100, 48_000), (96_000, 44_100)] {
            let input = sine_mono(from_sr, FREQUENCY_HZ, AMPLITUDE);
            let output = rubato_resample_batch_exact(&input, from_sr, to_sr, 1);
            let thd_n_db = thd_plus_noise_db(&output, to_sr, FREQUENCY_HZ);

            assert!(
                thd_n_db < -100.0,
                "THD+N du SRC {from_sr} -> {to_sr} Hz mesuree a {thd_n_db:.1} dB"
            );
        }
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

#[cfg(test)]
mod piste_entiere_tests {
    use super::*;

    const FROM_SR: u32 = 44_100;
    const TO_SR: u32 = 48_000;
    const CH: usize = 2;

    /// Piste stereo de `frames` trames, silencieuse sauf une impulsion
    /// pleine echelle a la trame `pic`.
    fn impulsion(frames: usize, pic: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; frames * CH];
        v[pic * CH] = 1.0;
        v[pic * CH + 1] = 1.0;
        v
    }

    fn trame_du_pic(sortie: &[f32]) -> usize {
        sortie
            .chunks(CH)
            .enumerate()
            .max_by(|a, b| a.1[0].abs().partial_cmp(&b.1[0].abs()).unwrap())
            .map(|(i, _)| i)
            .expect("sortie vide")
    }

    /// #2246 — mesure du temps, pas de l'intention.
    ///
    /// Le chemin local des formats compresses decode la piste ENTIERE en
    /// memoire puis adapte sa cadence. On y envoie une impulsion a une trame
    /// connue et on lit deux nombres en sortie :
    ///
    /// 1. le nombre de trames rendues — il doit valoir exactement
    ///    `round(trames_entree × ratio)` ;
    /// 2. la trame du pic — elle doit valoir `round(pic × ratio)`.
    ///
    /// Tant que cette fonction conserve le delai de groupe du sinc, les deux
    /// sont faux du meme nombre de trames : la piste s'allonge et son contenu
    /// glisse vers la droite. Le message d'echec imprime l'ecart mesure.
    #[test]
    fn la_piste_entiere_ne_glisse_ni_ne_s_allonge() {
        const TRAMES: usize = 44_100; // 1 s
        const PIC: usize = 4_410; // 100 ms

        let entree = impulsion(TRAMES, PIC);
        let sortie = rubato_resample_track(&entree, FROM_SR, TO_SR, CH as u16);

        let ratio = TO_SR as f64 / FROM_SR as f64;
        let trames_attendues = (TRAMES as f64 * ratio).round() as usize;
        let pic_attendu = (PIC as f64 * ratio).round() as usize;

        let trames_rendues = sortie.len() / CH;
        let pic_rendu = trame_du_pic(&sortie);

        let derive = pic_rendu as i64 - pic_attendu as i64;
        assert!(
            derive.abs() <= 1,
            "position : le pic est rendu a la trame {pic_rendu} au lieu de \
             {pic_attendu}, soit une derive de {derive} trames \
             ({:.3} ms a {TO_SR} Hz). Le delai de groupe du sinc est conserve \
             et decale toute la piste (#2246).",
            derive as f64 * 1000.0 / TO_SR as f64
        );

        assert_eq!(
            trames_rendues,
            trames_attendues,
            "longueur : {trames_rendues} trames rendues pour {trames_attendues} \
             attendues, soit {} trames en trop. La duree et la position \
             annoncees par le chemin compresse sont calculees sur cette \
             sortie : elles mentent d'autant (#2246).",
            trames_rendues as i64 - trames_attendues as i64
        );
    }

    /// #2246 — la contre-partie : le contrat du chemin en FLUX est l'inverse,
    /// et il ne doit pas etre « corrige ».
    ///
    /// `rubato_resample_batch` conserve deliberement delai et queue. Ce test
    /// fige cette difference : si un jour les deux fonctions rendaient la
    /// meme chose, c'est que l'une des deux aurait ete alignee sur l'autre
    /// par megarde.
    #[test]
    fn le_contrat_du_flux_reste_distinct_de_celui_de_la_piste() {
        const TRAMES: usize = 8_820; // 200 ms
        let entree = impulsion(TRAMES, 441);

        let flux = rubato_resample_batch(&entree, FROM_SR, TO_SR, CH as u16);
        let piste = rubato_resample_track(&entree, FROM_SR, TO_SR, CH as u16);

        assert!(
            flux.len() > piste.len(),
            "le contrat en flux doit rester plus long que le contrat piste : \
             {} contre {} echantillons",
            flux.len(),
            piste.len()
        );
    }

    /// #2246 — le chemin compresse de `outputs/local.rs` doit appeler le
    /// contrat « piste entiere ».
    ///
    /// Les deux tests ci-dessus mesurent la fonction ; celui-ci verifie que
    /// c'est bien elle que la production appelle. Meme controle grossier que
    /// `chemin_compresse_dsp_tests` (#1725) : il attrape la seule regression
    /// qui compte, quelqu'un qui rebranche la variante en flux.
    #[test]
    fn le_chemin_compresse_appelle_le_contrat_piste_entiere() {
        let source = include_str!("../outputs/local.rs");
        let branche = source
            .split("local_audio_compressed_playing")
            .nth(1)
            .expect("branche compressee introuvable");
        let avant_tampon = branche
            .split("Pre-fill the ring buffer")
            .next()
            .expect("pre-remplissage introuvable");

        assert!(
            avant_tampon.contains("rubato_resample_track("),
            "le chemin compresse detient la piste entiere : il doit appeler \
             rubato_resample_track, qui retire le delai de groupe. La \
             variante en flux ajoute ~2×delay trames A CHAQUE PISTE (#2246)."
        );
        assert!(
            !avant_tampon.contains("rubato_resample_batch("),
            "le chemin compresse appelle encore rubato_resample_batch : \
             delai de groupe et queue conserves a chaque piste (#2246)."
        );
    }
}
