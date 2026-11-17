//! Streaming click detector and interpolator, interleaved `f64`.
//!
//! See `docs/tess/declick-live.md`. Output is the input delayed by exactly
//! `latency_frames()`; only the samples of an accepted click are rewritten.
use crate::ar;

/// AR model order.
pub const ORDER: usize = 32;
/// Detected samples closer than this are merged into one candidate.
const GAP: usize = 8;
/// Samples added on each side of a candidate.
const GUARD: usize = 2;
/// A candidate is rejected when the local robust scale before or after it
/// exceeds this multiple of the reference scale (a transient, not a click).
pub const ISOLATION: f64 = 2.5;
/// Same, for the RMS after the candidate.
pub const RMS_ISOLATION: f64 = 1.6;
/// A candidate is rejected when a model fitted after it predicts the
/// signal that much better than the past model: the spectrum changed, a new
/// sound started.
pub const CHANGE_MAX: f64 = 3.0;
/// Fraction of the peak error that bounds the core of a click.
const CORE: f64 = 0.03;
/// Silence floor of the analysis window (mean square, about −120 dBFS).
const SILENCE: f64 = 1e-12;
/// Lowest noise floor assumed in the input: the quantization noise of
/// 16-bit PCM (LSB/√12, about −101 dBFS). A synthetic or 24-bit recording
/// can be far more predictable than that; without this floor any new note
/// would stand hundreds of scales above it. Nothing quieter is audible as
/// a click, and no vinyl transfer is that quiet.
const QUANTUM: f64 = 1.0 / 32_768.0 / 3.464_101_615_137_754_6;
/// Capacity of the event log read by diagnostics and tests.
pub const EVENT_CAPACITY: usize = 4096;

pub const LOOKAHEAD_MIN_MS: f64 = 10.0;
pub const LOOKAHEAD_MAX_MS: f64 = 20.0;
pub const THRESHOLD_MIN: f64 = 4.0;
pub const THRESHOLD_MAX: f64 = 30.0;
pub const MAX_CLICK_MIN_MS: f64 = 0.05;
pub const MAX_CLICK_MAX_MS: f64 = 2.0;
pub const RATE_MIN: u32 = 32_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Detection threshold, in robust standard deviations of the
    /// matched-filtered prediction error.
    pub threshold: f64,
    pub lookahead_ms: f64,
    /// Longest click repaired, not counting the `2p` matched-filter spread.
    pub max_click_ms: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: 18.0,
            lookahead_ms: 15.0,
            max_click_ms: 1.5,
        }
    }
}

/// Derived sizes, in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub lookahead: usize,
    pub hop: usize,
    pub analysis: usize,
    /// Past span of the reference scale.
    pub scale: usize,
    pub control: usize,
    pub max_region: usize,
}

impl Sizes {
    pub fn new(rate: u32, config: &Config) -> Option<Self> {
        let valid = rate >= RATE_MIN
            && (LOOKAHEAD_MIN_MS..=LOOKAHEAD_MAX_MS).contains(&config.lookahead_ms)
            && (THRESHOLD_MIN..=THRESHOLD_MAX).contains(&config.threshold)
            && (MAX_CLICK_MIN_MS..=MAX_CLICK_MAX_MS).contains(&config.max_click_ms);
        if !valid {
            return None;
        }
        let ms = |v: f64| (v * f64::from(rate) / 1000.0).round() as usize;
        let lookahead = ms(config.lookahead_ms);
        let analysis = ms(20.0).max(4 * ORDER);
        let scale = ms(5.0).max(4 * ORDER);
        let max_region = ms(config.max_click_ms).max(1) + 2 * ORDER + 2 * GUARD;
        // Budget after the hop: the longest candidate, its guard, 2p of
        // context for the matched filter, and the control window. A 4 ms
        // hop halves the analysis cost; a short lookahead falls back to 2 ms.
        let fixed = max_region + GUARD + GAP + 1 + 2 * ORDER;
        let (hop, budget) = [ms(4.0), ms(2.0)]
            .into_iter()
            .filter_map(|hop| Some((hop, lookahead.checked_sub(hop + fixed)?)))
            .find(|(_, budget)| *budget >= ms(2.0).max(32))
            .or_else(|| Some((ms(2.0), lookahead.checked_sub(ms(2.0) + fixed)?)))?;
        let control = budget.min(ms(3.0));
        (control >= 32).then_some(Self {
            lookahead,
            hop,
            analysis,
            scale,
            control,
            max_region,
        })
    }
}

/// A configuration or a previous instance this processor cannot take over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incompatible;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub clicks_repaired: u64,
    pub samples_modified: u64,
    pub rejected_long: u64,
    pub rejected_transient: u64,
    pub rejected_unresolved: u64,
    pub hops_silent: u64,
}

/// One repaired region, in input frames counted since the last reset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    pub frame: u64,
    pub len: u32,
    pub channel: u16,
    /// Peak of the matched-filtered error, in reference scales.
    pub peak: f32,
    /// Robust scale before and after the candidate, in reference scales.
    pub pre: f32,
    pub post: f32,
    /// RMS after the candidate, in reference scales.
    pub post_rms: f32,
    /// Spectral change after the candidate: prediction error power of the
    /// past model over that of a model fitted after the candidate.
    pub change: f32,
}

pub struct DeclickLive {
    channels: usize,
    config: Config,
    sizes: Sizes,
    rate: u32,
    mask: usize,
    ring: Vec<f64>,
    /// Frames received since reset (real and drain padding).
    t: u64,
    /// Real frames received since reset.
    real: u64,
    draining: bool,
    drained: u64,
    skip_until: Vec<u64>,
    stats: Stats,
    events: Vec<Event>,
    events_total: u64,
    // Scratch, allocated once.
    w: Vec<f64>,
    e: Vec<f64>,
    et: Vec<f64>,
    abs: Vec<f64>,
    window: Vec<f64>,
    xw: Vec<f64>,
    r: Vec<f64>,
    a: Vec<f64>,
    tmp: Vec<f64>,
    band: Vec<f64>,
    chol: Vec<f64>,
    ek: Vec<f64>,
    rhs: Vec<f64>,
    future_w: Vec<f64>,
    a_future: Vec<f64>,
}

impl DeclickLive {
    /// Allocates everything. `None` when the rate or settings are outside
    /// the supported range or the lookahead cannot hold the context needed.
    pub fn new(rate: u32, channels: usize, config: Config) -> Option<Self> {
        if !(1..=64).contains(&channels) {
            return None;
        }
        let sizes = Sizes::new(rate, &config)?;
        let span = sizes.analysis + sizes.lookahead;
        let cap = (span + sizes.hop + 1).next_power_of_two();
        let n = sizes.analysis;
        let window = (0..n)
            .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * (i as f64 + 0.5) / n as f64).cos())
            .collect();
        let max_m = max_region_capacity(rate);
        Some(Self {
            channels,
            config,
            sizes,
            rate,
            mask: cap - 1,
            ring: vec![0.0; cap * channels],
            t: 0,
            real: 0,
            draining: false,
            drained: 0,
            skip_until: vec![0; channels],
            stats: Stats::default(),
            events: Vec::with_capacity(EVENT_CAPACITY),
            events_total: 0,
            w: vec![0.0; span],
            e: vec![0.0; span],
            et: vec![0.0; span],
            abs: vec![0.0; span],
            window,
            xw: vec![0.0; n],
            r: vec![0.0; ORDER + 1],
            a: vec![0.0; ORDER + 1],
            tmp: vec![0.0; ORDER + 1],
            band: vec![0.0; ORDER + 1],
            chol: vec![0.0; max_m * (ORDER + 1)],
            ek: vec![0.0; max_m + ORDER],
            rhs: vec![0.0; max_m],
            future_w: vec![0.0; (8 * rate / 1000) as usize],
            a_future: vec![0.0; ORDER + 1],
        })
    }

    pub fn latency_frames(&self) -> usize {
        self.sizes.lookahead
    }
    pub fn sizes(&self) -> Sizes {
        self.sizes
    }
    pub fn config(&self) -> Config {
        self.config
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn stats(&self) -> Stats {
        self.stats
    }
    /// The first [`EVENT_CAPACITY`] repairs since reset.
    pub fn events(&self) -> &[Event] {
        &self.events
    }
    pub fn events_total(&self) -> u64 {
        self.events_total
    }

    /// Live change of threshold and maximum click length. The lookahead (and
    /// therefore the announced latency) cannot change on a live processor.
    pub fn set_config(&mut self, config: Config) -> Result<(), Incompatible> {
        if config.lookahead_ms != self.config.lookahead_ms {
            return Err(Incompatible);
        }
        let sizes = Sizes::new(self.rate, &config).ok_or(Incompatible)?;
        if sizes.max_region > max_region_capacity(self.rate) {
            return Err(Incompatible);
        }
        self.config = config;
        self.sizes = sizes;
        Ok(())
    }

    /// Takes over the delay line and position of a compatible instance.
    pub fn inherit_state_from(&mut self, previous: &Self) -> Result<(), Incompatible> {
        if previous.channels != self.channels
            || previous.rate != self.rate
            || previous.sizes.lookahead != self.sizes.lookahead
            || previous.ring.len() != self.ring.len()
        {
            return Err(Incompatible);
        }
        self.ring.copy_from_slice(&previous.ring);
        self.t = previous.t;
        self.real = previous.real;
        self.draining = previous.draining;
        self.drained = previous.drained;
        self.skip_until.copy_from_slice(&previous.skip_until);
        Ok(())
    }

    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.t = 0;
        self.real = 0;
        self.draining = false;
        self.drained = 0;
        self.skip_until.fill(0);
        self.stats = Stats::default();
        self.events.clear();
        self.events_total = 0;
    }

    /// In place: `io` (interleaved) receives the input delayed by the
    /// lookahead, clicks repaired. Frames after a drain started are refused
    /// by the caller (see the SDK wrapper).
    pub fn process(&mut self, io: &mut [f64]) {
        let ch = self.channels;
        let l = self.sizes.lookahead as u64;
        for frame in io.chunks_exact_mut(ch) {
            if self.t.is_multiple_of(self.sizes.hop as u64) {
                self.hop();
            }
            let wi = (self.t as usize) & self.mask;
            let ri = (self.t.wrapping_sub(l) as usize) & self.mask;
            for (c, v) in frame.iter_mut().enumerate() {
                self.ring[wi * ch + c] = *v;
                *v = if self.t >= l {
                    self.ring[ri * ch + c]
                } else {
                    0.0
                };
            }
            self.t += 1;
            if !self.draining {
                self.real += 1;
            }
        }
    }

    /// Writes up to `out.len() / channels` of the frames still held in the
    /// delay line. Returns the number of frames written and whether the
    /// line is empty.
    pub fn drain(&mut self, out: &mut [f64]) -> (usize, bool) {
        let ch = self.channels;
        let l = self.sizes.lookahead as u64;
        let pending = l.min(self.real);
        self.draining = true;
        let mut written = 0;
        let mut frame = [0.0f64; 64];
        while self.drained < pending && (written + 1) * ch <= out.len() {
            // Feed silence until the next frame to emit comes out.
            loop {
                let emits = self.t >= l && self.t - l >= self.real - pending;
                let f = &mut frame[..ch];
                f.fill(0.0);
                self.process(f);
                if emits {
                    out[written * ch..(written + 1) * ch].copy_from_slice(f);
                    break;
                }
            }
            written += 1;
            self.drained += 1;
        }
        (written, self.drained >= pending)
    }

    fn hop(&mut self) {
        let s = self.sizes;
        if self.t < (s.analysis + s.lookahead) as u64 {
            return;
        }
        for c in 0..self.channels {
            self.hop_channel(c);
        }
    }

    fn hop_channel(&mut self, c: usize) {
        let s = self.sizes;
        let p = ORDER;
        let (n, l, h) = (s.analysis, s.lookahead, s.hop);
        let span = n + l;
        let t = self.t;
        let base = t - span as u64;
        let ch = self.channels;
        for (j, slot) in self.w.iter_mut().enumerate() {
            *slot = self.ring[(((base + j as u64) as usize) & self.mask) * ch + c];
        }
        // 1. AR model on the restored past.
        let energy = self.w[..n].iter().map(|v| v * v).sum::<f64>() / n as f64;
        if energy < SILENCE {
            self.stats.hops_silent += 1;
            return;
        }
        for ((x, w), v) in self.xw.iter_mut().zip(&self.window).zip(&self.w[..n]) {
            *x = w * v;
        }
        ar::autocorrelation(&self.xw, &mut self.r);
        self.r[0] *= 1.0 + 1e-9;
        if ar::levinson(&self.r, &mut self.a, &mut self.tmp).is_none() {
            return;
        }
        ar::coefficient_autocorrelation(&self.a, &mut self.band);
        // 2. Prediction error and matched filter. Only real frames count:
        // drain padding never serves as "after" context.
        let real_end = (self.real.min(t) - base) as usize;
        // The errors are only needed from the reference-scale span on.
        let from = n - s.scale;
        for j in from - p..real_end {
            let mut acc = 0.0;
            for (k, ak) in self.a.iter().enumerate() {
                acc += ak * self.w[j - k];
            }
            self.e[j] = acc;
        }
        let et_end = real_end.saturating_sub(p);
        for j in from..et_end {
            let mut acc = 0.0;
            for (k, ak) in self.a.iter().enumerate() {
                acc += ak * self.e[j + k];
            }
            self.et[j] = acc;
        }
        // 3. Robust reference scale on the restored past.
        let sigma_floor = QUANTUM
            * (self.band[0] * self.band[0]
                + 2.0 * self.band[1..].iter().map(|v| v * v).sum::<f64>())
            .sqrt();
        let sigma = robust_scale(&self.et[from..n], &mut self.abs).max(sigma_floor);
        let theta = self.config.threshold * sigma;
        let sigma_e = robust_scale(&self.e[from..n], &mut self.abs);
        // 4. Candidates that start in F = [n, n + h).
        let Some(lim) = et_end.checked_sub(p + s.control + GUARD) else {
            return;
        };
        let skip = self.skip_until[c].saturating_sub(base) as usize;
        let mut j = n.max(skip);
        while j < (n + h).min(lim) {
            if self.et[j].abs() <= theta {
                j += 1;
                continue;
            }
            let start = j;
            let mut end = j + 1;
            let mut peak = self.et[j].abs();
            let mut k = j + 1;
            let mut resolved = false;
            while k < lim {
                if self.et[k].abs() > theta {
                    end = k + 1;
                    peak = peak.max(self.et[k].abs());
                } else if k - end >= GAP {
                    resolved = true;
                    break;
                }
                k += 1;
            }
            let rs = start.saturating_sub(GUARD).max(n);
            let re = end + GUARD;
            self.skip_until[c] = base + re as u64;
            j = re;
            if start == n && self.et[n - 1].abs() > theta {
                // Began in frames already played: leave it alone.
                self.stats.rejected_unresolved += 1;
                continue;
            }
            if !resolved {
                // Its end is not visible within the lookahead.
                self.stats.rejected_unresolved += 1;
                continue;
            }
            if re - rs > s.max_region {
                self.stats.rejected_long += 1;
                continue;
            }
            // 5. Isolation: a click is short and additive, the signal
            // around it stays as predictable as before. The robust scale
            // (median) before and after catches a change of regime; the RMS
            // after catches sparse spike trains that the median ignores,
            // such as the corners of a newly plucked string.
            let pw = &self.et[re + p..re + p + s.control];
            let post_rms = (pw.iter().map(|v| v * v).sum::<f64>() / pw.len() as f64).sqrt();
            let post = robust_scale(pw, &mut self.abs);
            let pre_end = rs - p;
            let pre_start = pre_end.saturating_sub(s.control).max(from);
            let pre = if pre_end > pre_start {
                robust_scale(&self.et[pre_start..pre_end], &mut self.abs)
            } else {
                0.0
            };
            let local = sigma.max(pre).max(post).max(post_rms);
            if post > ISOLATION * sigma
                || pre > ISOLATION * sigma
                || post_rms > RMS_ISOLATION * sigma
                || peak < self.config.threshold * local
            {
                self.stats.rejected_transient += 1;
                continue;
            }
            let change = self.spectral_change(re + p, et_end);
            if change > CHANGE_MAX {
                self.stats.rejected_transient += 1;
                continue;
            }
            // 6. Core of the click. The matched filter spreads it `p` samples
            // each way; the corrupted samples themselves start where the
            // forward error starts and end where the backward error ends.
            // Interpolating only them keeps every clean sample as context.
            let (cs, ce) = self.core(start, end, real_end, 3.0 * sigma_e);
            let (rs, re) = (cs.saturating_sub(GUARD).max(rs), (ce + GUARD).min(re));
            // 7. LSAR interpolation.
            if !ar::interpolate(
                &mut self.w,
                rs,
                re,
                &self.a,
                &self.band,
                &mut self.chol,
                &mut self.ek,
                &mut self.rhs,
            ) {
                self.stats.rejected_unresolved += 1;
                continue;
            }
            for idx in rs..re {
                self.ring[(((base + idx as u64) as usize) & self.mask) * ch + c] = self.w[idx];
            }
            self.stats.clicks_repaired += 1;
            self.stats.samples_modified += (re - rs) as u64;
            self.events_total += 1;
            if self.events.len() < EVENT_CAPACITY {
                self.events.push(Event {
                    frame: base + rs as u64,
                    len: (re - rs) as u32,
                    channel: c as u16,
                    peak: (peak / sigma) as f32,
                    pre: (pre / sigma) as f32,
                    post: (post / sigma) as f32,
                    post_rms: (post_rms / sigma) as f32,
                    change: change as f32,
                });
            }
        }
    }
}

impl DeclickLive {
    /// Prediction error power of the current (past) model over that of a
    /// model fitted on `w[from..]` (at most 8 ms, up to `until`). A click
    /// leaves the spectrum as it was: about 1. A new note changes it.
    fn spectral_change(&mut self, from: usize, until: usize) -> f64 {
        let p = ORDER;
        let len = until.saturating_sub(from).min(self.future_w.len());
        if len < 4 * p {
            return 1.0;
        }
        let seg = &self.w[from..from + len];
        for (i, (x, v)) in self.future_w[..len].iter_mut().zip(seg).enumerate() {
            let hann = 0.5 - 0.5 * (std::f64::consts::TAU * (i as f64 + 0.5) / len as f64).cos();
            *x = hann * v;
        }
        ar::autocorrelation(&self.future_w[..len], &mut self.r);
        self.r[0] *= 1.0 + 1e-9;
        if ar::levinson(&self.r, &mut self.a_future, &mut self.tmp).is_none() {
            return 1.0;
        }
        let (mut old, mut new) = (0.0, 0.0);
        for j in from + p..from + len {
            let (mut eo, mut en) = (0.0, 0.0);
            for k in 0..=p {
                eo += self.a[k] * self.w[j - k];
                en += self.a_future[k] * self.w[j - k];
            }
            old += eo * eo;
            new += en * en;
        }
        if new > 0.0 { old / new } else { 1.0 }
    }

    /// `[first, last + 1)` of the corrupted samples of a run `[start, end)`
    /// of the matched-filtered error: the first sample where the forward
    /// error reaches [`CORE`] of its maximum (and `floor`, three robust
    /// scales of the error), the last where the backward error does. The
    /// forward error only spreads AFTER the click and the backward error
    /// only BEFORE it, so neither spread moves these bounds. Falls back to
    /// the run if the two disagree.
    fn core(&self, start: usize, end: usize, real_end: usize, floor: f64) -> (usize, usize) {
        let p = ORDER;
        let fwd = start..(end + p).min(real_end);
        let max_f = self.e[fwd.clone()]
            .iter()
            .fold(0.0f64, |m, v| m.max(v.abs()));
        let first = fwd
            .clone()
            .find(|&j| self.e[j].abs() >= (CORE * max_f).max(floor));
        let backward = |j: usize| -> f64 {
            self.a
                .iter()
                .enumerate()
                .map(|(k, ak)| ak * self.w[j + k])
                .sum()
        };
        let bwd = start.saturating_sub(p).max(p)..end;
        let mut max_b = 0.0f64;
        for j in bwd.clone() {
            max_b = max_b.max(backward(j).abs());
        }
        let last = bwd
            .rev()
            .find(|&j| backward(j).abs() >= (CORE * max_b).max(floor));
        match (first, last) {
            (Some(f), Some(l)) if f <= l => (f, l + 1),
            _ => (start, end),
        }
    }
}

/// Largest candidate over every setting at this rate.
fn max_region_capacity(rate: u32) -> usize {
    (MAX_CLICK_MAX_MS * f64::from(rate) / 1000.0).round() as usize + 2 * ORDER + 2 * GUARD
}

/// `1.4826 × median |x|`, using `scratch` (no allocation).
fn robust_scale(x: &[f64], scratch: &mut [f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let s = &mut scratch[..x.len()];
    for (d, v) in s.iter_mut().zip(x) {
        *d = v.abs();
    }
    let mid = s.len() / 2;
    let (_, m, _) = s.select_nth_unstable_by(mid, f64::total_cmp);
    1.4826 * *m
}
