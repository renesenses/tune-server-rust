//! Historical trim algorithm. Zero crossing scans the entire silent lead/tail;
//! the old “50 ms” comment did not match its implementation.
use crate::TrimOptions;
pub fn trim_window(
    samples: &[i32],
    channels: usize,
    bit_depth: u16,
    opts: TrimOptions,
) -> Result<std::ops::Range<usize>, String> {
    if channels == 0
        || ![16, 24, 32].contains(&bit_depth)
        || !samples.len().is_multiple_of(channels)
        || !opts.threshold_db.is_finite()
    {
        return Err("invalid PCM or threshold".into());
    }
    let total_frames = samples.len() / channels;
    if total_frames == 0 {
        return Err("decoded audio is empty".into());
    }
    // samples_i32 are RIGHT-JUSTIFIED at `bit_depth` (a 16-bit sample lives in
    // bits 0..15, 24-bit in 0..23), so digital full scale is 2^(bit_depth-1).
    // Linear silence threshold amplitude = 10^(dB/20) * full_scale.
    let full_scale = (1i64 << (bit_depth.saturating_sub(1)).max(1)) as f64;
    let threshold_lin = 10f64.powf(opts.threshold_db as f64 / 20.0) * full_scale;

    // A frame is "loud" if ANY channel exceeds the threshold.
    let frame_is_loud = |f: usize| -> bool {
        let base = f * channels;
        for c in 0..channels {
            if (samples[base + c].unsigned_abs() as f64) > threshold_lin {
                return true;
            }
        }
        false
    };

    // Leading edge.
    let mut lead_start = 0usize;
    if opts.trim_lead {
        match (0..total_frames).find(|&f| frame_is_loud(f)) {
            Some(f) => lead_start = f,
            None => {
                // Whole track is below threshold: nothing meaningful to keep.
                return Err("track is entirely below the silence threshold".to_string());
            }
        }
        if opts.zero_cross && lead_start > 0 {
            lead_start = snap_zero_crossing_back(samples, channels, lead_start);
        }
    }

    // Trailing edge (inclusive frame index of the last kept frame).
    let mut tail_end = total_frames - 1;
    if opts.trim_tail {
        match (0..total_frames).rev().find(|&f| frame_is_loud(f)) {
            Some(f) => tail_end = f,
            None => {
                return Err("track is entirely below the silence threshold".to_string());
            }
        }
        if opts.zero_cross && tail_end + 1 < total_frames {
            tail_end = snap_zero_crossing_fwd(samples, channels, tail_end, total_frames);
        }
    }

    // Validate the window before slicing — never emit empty/garbage output.
    if lead_start > tail_end {
        return Err(format!(
            "invalid trim window (lead_start {lead_start} > tail_end {tail_end})"
        ));
    }

    let slice = &samples[lead_start * channels..(tail_end + 1) * channels];
    if slice.is_empty() {
        return Err("trimmed audio is empty".to_string());
    }

    Ok(lead_start * channels..(tail_end + 1) * channels)
}
/// Move a leading edge earlier to the nearest zero crossing on channel 0, so the
/// cleaned file starts on a zero-valued sample rather than a step (the "ploc").
/// Searches the silent lead-in; returns the original index if none is found.
fn snap_zero_crossing_back(samples: &[i32], channels: usize, start: usize) -> usize {
    let window = start; // scan the whole lead-in silence; it's short by construction
    let ch0 = |f: usize| samples[f * channels];
    let mut f = start;
    let lo = start.saturating_sub(window);
    while f > lo {
        let cur = ch0(f);
        let prev = ch0(f - 1);
        if cur == 0 {
            return f;
        }
        // Sign change between prev and cur → crossing sits at f.
        if (prev <= 0 && cur >= 0) || (prev >= 0 && cur <= 0) {
            return f;
        }
        f -= 1;
    }
    start
}

/// Extend a trailing edge later to the nearest zero crossing on channel 0, so the
/// cleaned file ends on a zero-valued sample rather than a step.
fn snap_zero_crossing_fwd(
    samples: &[i32],
    channels: usize,
    end: usize,
    total_frames: usize,
) -> usize {
    let ch0 = |f: usize| samples[f * channels];
    let mut f = end;
    while f + 1 < total_frames {
        let cur = ch0(f);
        let next = ch0(f + 1);
        if cur == 0 {
            return f;
        }
        if (cur <= 0 && next >= 0) || (cur >= 0 && next <= 0) {
            return f;
        }
        f += 1;
    }
    end
}
