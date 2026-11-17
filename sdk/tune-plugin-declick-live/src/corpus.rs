//! Synthetic test material, generated (no protected recordings): the click
//! corpus and the hard clean material of the counter-proof. Deterministic for
//! a given seed. Mono generators; [`stereo`] builds a decorrelated pair.
use std::f64::consts::TAU;

/// xorshift64* — small, deterministic, no dependency.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    /// Approximately normal (sum of 4 uniforms).
    pub fn gauss(&mut self) -> f64 {
        (self.unit() + self.unit() + self.unit() + self.unit() - 2.0) * 1.732_050_8
    }
}

/// Damper release: real instruments never stop on a step (a step IS a
/// click). Raised-cosine fade over the last `release` frames of a note.
fn release(i: usize, dur: usize, release: usize) -> f64 {
    let left = dur.saturating_sub(i);
    if left >= release {
        1.0
    } else {
        0.5 - 0.5 * (std::f64::consts::PI * left as f64 / release as f64).cos()
    }
}

fn normalize(x: &mut [f64], peak: f64) {
    let m = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if m > 0.0 {
        for v in x {
            *v *= peak / m;
        }
    }
}

/// Harpsichord: Karplus-Strong plucks, fast arpeggios over the whole compass (FF1 – f3, 44 Hz – 1.4 kHz),
/// kinked attacks (triangular plucked-string shape, bright), many
/// overlapping voices.
pub fn harpsichord(rate: u32, seconds: f64, seed: u64) -> Vec<f64> {
    harpsichord_notes(rate, seconds, seed).0
}

/// [`harpsichord`] and its notes: (start frame, duration, f0 in Hz).
pub fn harpsichord_notes(
    rate: u32,
    seconds: f64,
    seed: u64,
) -> (Vec<f64>, Vec<(usize, usize, f64)>) {
    let mut notes = Vec::new();
    let fs = f64::from(rate);
    let len = (seconds * fs) as usize;
    let mut out = vec![0.0; len];
    let mut rng = Rng::new(seed);
    let mut start = (0.05 * fs) as usize;
    while start < len {
        // The harpsichord compass: FF1 (43.7 Hz) to f3 (1397 Hz).
        let midi = rng.range(29.0, 89.0);
        let f0 = 440.0 * 2f64.powf((midi - 69.0) / 12.0);
        let period = (fs / f0).max(2.0);
        let n = period as usize;
        // Plucked string: the initial displacement is a triangle peaking at
        // the plucking point (8 – 20 % of the length), zero at both ends,
        // plus 5 % of noise. The attack is a kink, as sharp as the string
        // allows, but the displacement is continuous: a jump of the output
        // would be a click of the synthesis, not of the instrument.
        let beta = rng.range(0.08, 0.2);
        let mut line: Vec<f64> = (0..n)
            .map(|i| {
                let u = i as f64 / n as f64;
                let tri = if u < beta {
                    u / beta
                } else {
                    (1.0 - u) / (1.0 - beta)
                };
                tri * (1.0 + 0.05 * rng.range(-1.0, 1.0))
            })
            .collect();
        let damp = rng.range(0.994, 0.9995);
        let gain = rng.range(0.3, 1.0);
        let dur = ((rng.range(0.4, 2.5)) * fs) as usize;
        notes.push((start, dur, f0));
        let mut idx = 0;
        let fade = (0.03 * fs) as usize;
        // DC blocker (20 Hz), as in any recording chain: the triangle is
        // one-sided.
        let r = 1.0 - std::f64::consts::TAU * 20.0 / fs;
        let (mut px, mut py) = (0.0, 0.0);
        for i in 0..dur.min(len - start) {
            let next = line[(idx + 1) % n];
            let y = line[idx];
            let hp = y - px + r * py;
            px = y;
            py = hp;
            out[start + i] += gain * hp * release(i, dur, fade);
            // Brightness kept high: weighted average, mostly the current tap.
            line[idx] = damp * (0.7 * y + 0.3 * next);
            idx = (idx + 1) % n;
        }
        start += (rng.range(0.04, 0.18) * fs) as usize;
    }
    normalize(&mut out, 0.8);
    (out, notes)
}

/// Cymbals: crash and ride hits, zero-time attack, dense inharmonic partials
/// (300 Hz – 16 kHz) plus broadband noise, exponential decays.
pub fn cymbals(rate: u32, seconds: f64, seed: u64) -> Vec<f64> {
    let fs = f64::from(rate);
    let len = (seconds * fs) as usize;
    let mut out = vec![0.0; len];
    let mut rng = Rng::new(seed);
    let mut start = (0.05 * fs) as usize;
    let nyq = fs / 2.0 * 0.9;
    while start < len {
        let crash = rng.unit() < 0.35;
        let decay = if crash {
            rng.range(0.8, 2.0)
        } else {
            rng.range(0.15, 0.5)
        };
        let gain = if crash { 1.0 } else { rng.range(0.2, 0.5) };
        let partials: Vec<(f64, f64, f64)> = (0..60)
            .map(|_| {
                let f = (300.0 * (16_000.0f64 / 300.0).powf(rng.unit())).min(nyq);
                (f, rng.range(0.0, TAU), rng.range(0.2, 1.0))
            })
            .collect();
        let dur = ((decay * 5.0) * fs) as usize;
        let fade = (0.05 * fs) as usize;
        let mut lp = 0.0;
        for i in 0..dur.min(len - start) {
            let t = i as f64 / fs;
            let env = (-t / decay).exp();
            let mut v = 0.0;
            for &(f, ph, a) in &partials {
                v += a * (TAU * f * t + ph).sin();
            }
            let noise = rng.gauss();
            lp += 0.3 * (noise - lp);
            v = v / 20.0 + 0.6 * (noise - lp);
            out[start + i] += gain * env * v * release(i, dur, fade);
        }
        start += (rng.range(0.12, 0.6) * fs) as usize;
    }
    normalize(&mut out, 0.8);
    out
}

/// Piano: inharmonic partials (B ≈ 1e-4 – 4e-4) shaped by a felt-hammer
/// low-pass, 1 – 3 ms attacks, hammer thump, chords and runs from A0 to C8,
/// partial-dependent decays.
pub fn piano(rate: u32, seconds: f64, seed: u64) -> Vec<f64> {
    piano_notes(rate, seconds, seed).0
}

/// [`piano`] and its notes: (start frame, duration, f0 in Hz).
pub fn piano_notes(rate: u32, seconds: f64, seed: u64) -> (Vec<f64>, Vec<(usize, usize, f64)>) {
    let mut notes = Vec::new();
    let fs = f64::from(rate);
    let len = (seconds * fs) as usize;
    let mut out = vec![0.0; len];
    let mut rng = Rng::new(seed);
    let mut start = (0.05 * fs) as usize;
    let nyq = fs / 2.0 * 0.95;
    while start < len {
        let voices = if rng.unit() < 0.3 { 3 } else { 1 };
        for _ in 0..voices {
            let midi = rng.range(21.0, 108.0);
            let f0 = 440.0 * 2f64.powf((midi - 69.0) / 12.0);
            let b = rng.range(1e-4, 4e-4);
            let vel = rng.range(0.2, 1.0);
            let dur = (rng.range(0.5, 3.0) * fs) as usize;
            notes.push((start, dur, f0));
            let attack = (rng.range(0.001, 0.003) * fs).max(1.0);
            // Felt hammer: a velocity-dependent low-pass on the partials.
            let felt = 1_000.0 + 5_000.0 * vel;
            let partials: Vec<(f64, f64, f64)> = (1..=40)
                .map(|k| {
                    let k = k as f64;
                    let f = k * f0 * (1.0 + b * k * k).sqrt();
                    let lp = 1.0 / (1.0 + (f / felt).powi(2));
                    (f, lp * vel / k.powf(1.2 - 0.4 * vel), 0.3 + 3.0 / k)
                })
                .filter(|p| p.0 < nyq)
                .collect();
            let mut thump = 0.0;
            let fade = (0.06 * fs) as usize;
            for i in 0..dur.min(len - start) {
                let t = i as f64 / fs;
                let rise = if (i as f64) < attack {
                    0.5 - 0.5 * (std::f64::consts::PI * i as f64 / attack).cos()
                } else {
                    1.0
                };
                let mut v = 0.0;
                for &(f, a, d) in &partials {
                    v += a * (-t / d).exp() * (TAU * f * t).sin();
                }
                if (i as f64) < 3.0 * attack {
                    // Low thump of the hammer and keybed (below ~400 Hz).
                    thump += 0.05 * (rng.gauss() - thump);
                    v += 0.1 * vel * thump * (1.0 - i as f64 / (3.0 * attack));
                }
                out[start + i] += rise * v * release(i, dur, fade);
            }
        }
        start += (rng.range(0.08, 0.5) * fs) as usize;
    }
    normalize(&mut out, 0.8);
    (out, notes)
}

/// Mixed programme for the click corpus: piano, a sustained pad and a soft
/// surface noise floor (about −60 dBFS), around −20 dBFS RMS.
pub fn programme(rate: u32, seconds: f64, seed: u64) -> Vec<f64> {
    let fs = f64::from(rate);
    let mut out = piano(rate, seconds, seed);
    let mut rng = Rng::new(seed ^ 0xABCD);
    let chord: Vec<(f64, f64)> = (0..4)
        .map(|_| (110.0 * 2f64.powf(rng.range(0.0, 3.0)), rng.range(0.0, TAU)))
        .collect();
    for (i, v) in out.iter_mut().enumerate() {
        let t = i as f64 / fs;
        let pad: f64 = chord
            .iter()
            .map(|&(f, ph)| (TAU * f * t + ph).sin() * (0.5 + 0.5 * (TAU * 0.2 * t).sin()))
            .sum();
        *v = 0.5 * *v + 0.04 * pad + 0.001 * rng.gauss();
    }
    normalize(&mut out, 0.5);
    out
}

/// Interleaves two decorrelated takes of `generator` (seed and seed + 1).
pub fn stereo(
    generator: fn(u32, f64, u64) -> Vec<f64>,
    rate: u32,
    seconds: f64,
    seed: u64,
) -> Vec<f64> {
    let l = generator(rate, seconds, seed);
    let r = generator(rate, seconds, seed.wrapping_add(1));
    l.iter().zip(&r).flat_map(|(a, b)| [*a, *b]).collect()
}

/// One injected click.
#[derive(Debug, Clone, Copy)]
pub struct Click {
    pub frame: usize,
    pub channel: usize,
    /// Duration in frames (1 = single-sample impulse).
    pub width: usize,
    /// Peak amplitude, full scale = 1.
    pub amplitude: f64,
}

/// Adds `count` clicks to interleaved `signal`: half single-sample
/// impulses, half damped oscillations of 0.05 to 1 ms; peaks −40 to −6 dBFS,
/// at least 50 ms apart, in one channel or both (as on vinyl).
pub fn inject_clicks(
    signal: &mut [f64],
    channels: usize,
    rate: u32,
    count: usize,
    seed: u64,
) -> Vec<Click> {
    let fs = f64::from(rate);
    let frames = signal.len() / channels;
    let mut rng = Rng::new(seed);
    let spacing = frames / (count + 1);
    assert!(
        spacing as f64 >= 0.05 * fs,
        "clicks too dense for the corpus"
    );
    let mut clicks = Vec::new();
    for i in 0..count {
        let jitter = (rng.unit() * 0.5 * spacing as f64) as usize;
        let frame = (i + 1) * spacing - spacing / 4 + jitter;
        let impulse = i % 2 == 0;
        let width = if impulse {
            1
        } else {
            (rng.range(0.05, 1.0) * fs / 1000.0).max(2.0) as usize
        };
        let amplitude = 10f64.powf(rng.range(-40.0, -6.0) / 20.0);
        let sign = if rng.unit() < 0.5 { -1.0 } else { 1.0 };
        let freq = rng.range(3_000.0, 8_000.0).min(fs * 0.4);
        let both = rng.unit() < 0.5;
        let first = (rng.unit() * channels as f64) as usize % channels;
        for c in 0..channels {
            if !both && c != first {
                continue;
            }
            let amp = if both && c != first {
                amplitude * rng.range(0.5, 1.0)
            } else {
                amplitude
            };
            for k in 0..width {
                let v = if impulse {
                    1.0
                } else {
                    let t = k as f64 / width as f64;
                    (-4.0 * t).exp() * (TAU * freq * k as f64 / fs).cos()
                };
                signal[(frame + k) * channels + c] += sign * amp * v;
            }
            clicks.push(Click {
                frame,
                channel: c,
                width,
                amplitude: amp,
            });
        }
    }
    clicks
}
