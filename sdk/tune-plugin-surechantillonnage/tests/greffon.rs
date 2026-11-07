//! #7614 — the upsampler through the SDK's rate-changing stage contract.
use serde_json::json;
use tune_plugin_sdk::{Error, Settings, audio::*};
use tune_plugin_surechantillonnage::conception::par_id;
use tune_plugin_surechantillonnage::sdk::{
    OFFERED_FILTERS, Upsampler, choose_rate, choose_rate_for, headroom_db, headroom_gain,
    machine_reference_pair, settings as parse_settings, worst_case_gain,
};
use tune_plugin_testkit::render_rate_f32;

fn context(pure: bool) -> PlaybackContext {
    PlaybackContext {
        zone_id: 3,
        source: SourceKind::Library,
        delivery: Delivery::Local,
        pure,
        protected_bitstream: false,
    }
}

fn stereo(rate: u32) -> AudioFormat {
    AudioFormat::new(rate, ChannelLayout::Stereo, SampleEncoding::F32).unwrap()
}

/// 1 kHz on the left, 5 kHz on the right, −1 dBFS.
fn sines(rate: u32, frames: usize) -> Vec<f32> {
    let a = 0.891_f64;
    (0..frames)
        .flat_map(|n| {
            let t = n as f64 / f64::from(rate);
            [
                (a * (std::f64::consts::TAU * 1000.0 * t).sin()) as f32,
                (a * (std::f64::consts::TAU * 5000.0 * t).sin()) as f32,
            ]
        })
        .collect()
}

const ACCEPTED: [u32; 7] = [44_100, 48_000, 88_200, 96_000, 176_400, 352_800, 384_000];

#[test]
fn disabled_by_default_nothing_changes() {
    for settings in [Settings::Null, json!({})] {
        let out = render_rate_f32(
            &Upsampler,
            context(false),
            true,
            &settings,
            stereo(44_100),
            &ACCEPTED,
            &sines(44_100, 1024),
            256,
        )
        .unwrap();
        assert!(
            out.is_none(),
            "default settings must leave the stream untouched (settings {settings})"
        );
    }
}

#[test]
fn pure_never_instantiates_the_upsampler_even_when_enabled() {
    let out = render_rate_f32(
        &Upsampler,
        context(true),
        true,
        &json!({"enabled": true}),
        stereo(44_100),
        &ACCEPTED,
        &sines(44_100, 1024),
        256,
    )
    .unwrap();
    assert!(out.is_none());
}

#[test]
fn rate_follows_the_source_family_and_the_ceiling() {
    assert_eq!(choose_rate(44_100, &ACCEPTED, None), Some(352_800));
    assert_eq!(choose_rate(48_000, &ACCEPTED, None), Some(384_000));
    assert_eq!(choose_rate(48_000, &ACCEPTED, Some(192_000)), Some(96_000));
    assert_eq!(
        choose_rate(44_100, &[384_000, 768_000], None),
        None,
        "no integer multiple: the family is followed, no rational stage"
    );
    assert_eq!(choose_rate(44_100, &[1_411_200], None), None, "above ×16");
    assert_eq!(
        choose_rate(352_800, &ACCEPTED, None),
        None,
        "nothing higher"
    );
}

#[test]
fn bench_only_and_unknown_filters_are_refused() {
    for filter in ["precision-longue", "precision-longue-pm", "inconnu"] {
        let out = render_rate_f32(
            &Upsampler,
            context(false),
            true,
            &json!({"enabled": true, "filter": filter}),
            stereo(44_100),
            &ACCEPTED,
            &sines(44_100, 64),
            64,
        );
        assert!(matches!(out, Err(Error::InvalidSettings)), "{filter}");
    }
}

/// A sine through the linear-phase filters, once the kernel delay is
/// removed, equals the same sine at the output rate.
#[test]
fn enabled_upsampler_reproduces_in_band_sines_at_the_output_rate() {
    let frames = 8_820; // 0.2 s
    let input = sines(44_100, frames);
    for (filter, floor_db) in [("polyvalent", -95.0), ("precision", -110.0)] {
        let g = headroom_gain(filter, 8).unwrap();
        let cap = render_rate_f32(
            &Upsampler,
            context(false),
            true,
            &json!({"enabled": true, "filter": filter}),
            stereo(44_100),
            &ACCEPTED,
            &input,
            1024,
        )
        .unwrap()
        .expect("enabled, a higher integer rate exists");
        assert_eq!(cap.output_format.sample_rate(), 352_800);
        assert_eq!(cap.processed_frames, 8 * frames as u64);
        let total = cap.samples.len() / 2;
        assert_eq!(
            total as u64,
            8 * frames as u64 + u64::from(cap.latency_frames)
        );
        let kernel = par_id(filter).unwrap().noyau(8).len();
        let delay = (kernel as f64 - 1.0) / 2.0;
        // Compare past the start-up transient and before the drained end.
        let (from, to) = (kernel, 8 * frames - kernel);
        let (mut err, mut sig) = (0.0f64, 0.0f64);
        for k in from..to {
            let t = (k as f64 - delay) / 352_800.0;
            for (ch, f) in [(0usize, 1000.0), (1, 5000.0)] {
                let want = 0.891 * g * (std::f64::consts::TAU * f * t).sin();
                let got = f64::from(cap.samples[2 * k + ch]);
                err += (got - want).powi(2);
                sig += want * want;
            }
        }
        // The stage applies its constant headroom (L1 bound): `want` is the
        // attenuated sine.
        let rel_db = 10.0 * (err / sig).log10();
        assert!(
            rel_db < floor_db,
            "{filter}: error {rel_db:.1} dB relative to the sines (limit {floor_db} dB)"
        );
    }
}

#[test]
fn minimum_phase_runs_with_a_short_latency() {
    let cap = render_rate_f32(
        &Upsampler,
        context(false),
        true,
        &json!({"enabled": true, "filter": "precision-pm", "max_rate_hz": 192_000}),
        stereo(48_000),
        &ACCEPTED,
        &sines(48_000, 4096),
        512,
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap.output_format.sample_rate(), 96_000);
    // Linear phase would put the peak near 1 024 output frames (10.7 ms).
    assert!(
        cap.latency_frames < 64,
        "minimum phase latency {} frames",
        cap.latency_frames
    );
}

fn render(filter: &str, settings_extra: serde_json::Value, rate: u32, input: &[f32]) -> Vec<f32> {
    let mut settings = json!({"enabled": true, "filter": filter});
    if let (Some(o), Some(extra)) = (settings.as_object_mut(), settings_extra.as_object()) {
        o.extend(extra.clone());
    }
    render_rate_f32(
        &Upsampler,
        context(false),
        true,
        &settings,
        stereo(rate),
        &ACCEPTED,
        input,
        1024,
    )
    .unwrap()
    .expect("enabled")
    .samples
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Bertrand, 07/11 : réserve par filtre sur la norme L1 du polyphase. The
/// input that reaches the bound — the signs of the worst phase, reversed in
/// time, at full scale — must come out at or below full scale, and close to
/// it (the bound is tight: a smaller reserve would clip).
#[test]
fn worst_case_full_scale_input_never_exceeds_full_scale() {
    for filter in OFFERED_FILTERS {
        let l = 8usize;
        let kernel = par_id(filter).unwrap().noyau(l);
        let span = kernel.len() / l;
        let g = worst_case_gain(&kernel, l);
        let worst_phase = (0..l)
            .max_by(|&a, &b| {
                let s = |p: usize| kernel.iter().skip(p).step_by(l).map(|c| c.abs()).sum::<f64>();
                s(a).total_cmp(&s(b))
            })
            .unwrap();
        let n0 = span + 16;
        let frames = n0 + span + 64;
        let mut mono = vec![0.0f32; frames];
        for q in 0..span {
            let c = kernel[worst_phase + l * q];
            mono[n0 - q] = if c >= 0.0 { 1.0 } else { -1.0 };
        }
        let input: Vec<f32> = mono.iter().flat_map(|&x| [x, -x]).collect();
        let out = render(filter, json!({}), 44_100, &input);
        let p = peak(&out);
        assert!(p <= 1.0, "{filter}: peak {p} above full scale");
        if g > 1.0 {
            assert!(
                p > 0.999,
                "{filter}: the worst case should reach full scale (peak {p}, bound {g})"
            );
        }
    }
}

/// The classic inter-sample over: fs/4 at 45°, samples at ±1 (0 dBFS), whose
/// reconstruction peaks at +3.01 dBFS. And full-scale square waves.
#[test]
fn inter_sample_overs_and_squares_do_not_clip() {
    let fs4: Vec<f32> = (0..4096)
        .map(|n| [1.0f32, 1.0, -1.0, -1.0][n % 4])
        .flat_map(|x| [x, x])
        .collect();
    let square: Vec<f32> = (0..4096)
        .map(|n| if (n / 37) % 2 == 0 { 1.0f32 } else { -1.0 })
        .flat_map(|x| [x, -x])
        .collect();
    for filter in OFFERED_FILTERS {
        for (name, input) in [("fs/4 à 45°", &fs4), ("carré", &square)] {
            let out = render(filter, json!({}), 44_100, input);
            let p = peak(&out);
            assert!(p <= 1.0, "{filter} {name}: peak {p} above full scale");
        }
    }
}

/// Images of a 1 kHz sine at k·44.1 ± 1 kHz must be rejected by the
/// image-rejecting filters (the `resolution` family lets images through by
/// design, study § 3.2).
#[test]
fn images_of_an_in_band_sine_are_rejected() {
    let frames = 44_100 / 4;
    let input: Vec<f32> = (0..frames)
        .map(|n| (0.5 * (std::f64::consts::TAU * 1000.0 * n as f64 / 44_100.0).sin()) as f32)
        .flat_map(|x| [x, x])
        .collect();
    for (filter, limit_db) in [
        ("polyvalent", -100.0),
        ("polyvalent-pm", -100.0),
        ("precision", -120.0),
        ("precision-pm", -120.0),
    ] {
        let out = render(filter, json!({}), 44_100, &input);
        let left: Vec<f64> = out.iter().step_by(2).map(|&s| f64::from(s)).collect();
        // Steady state, Hann-windowed single-bin DFT (Goertzel form).
        let skip = 8192;
        let seg = &left[skip..skip + 65_536];
        let level = |f: f64| -> f64 {
            let n = seg.len() as f64;
            let w = std::f64::consts::TAU * f / 352_800.0;
            let (mut re, mut im) = (0.0, 0.0);
            for (k, &x) in seg.iter().enumerate() {
                let hann = 0.5 - 0.5 * (std::f64::consts::TAU * k as f64 / n).cos();
                re += x * hann * (w * k as f64).cos();
                im -= x * hann * (w * k as f64).sin();
            }
            (re * re + im * im).sqrt()
        };
        let fundamental = level(1000.0);
        for k in 1..=4u32 {
            for f in [f64::from(k) * 44_100.0 - 1000.0, f64::from(k) * 44_100.0 + 1000.0] {
                let rel = 20.0 * (level(f) / fundamental).log10();
                assert!(
                    rel < limit_db,
                    "{filter}: image at {f} Hz at {rel:.1} dB (limit {limit_db} dB)"
                );
            }
        }
    }
}

/// The level drop of each filter, as reported (and pinned: never above 0 dB).
#[test]
fn headroom_is_a_drop_never_a_gain() {
    for filter in OFFERED_FILTERS {
        for ratio in [2u32, 4, 8, 16] {
            let db = headroom_db(filter, ratio).unwrap();
            println!("headroom {filter} x{ratio}: {db:.3} dB");
            assert!(db <= 0.0, "{filter} ×{ratio}: {db} dB");
            assert!(db > -20.0, "{filter} ×{ratio}: {db} dB is implausible");
        }
    }
    assert!(headroom_gain("precision-longue", 8).is_none());
    assert!(headroom_gain("polyvalent", 3).is_some());
    assert!(headroom_gain("polyvalent", 32).is_none());
}

#[test]
fn target_ratio_caps_the_output_rate() {
    let s = parse_settings(&json!({"enabled": true, "max_ratio": 4})).unwrap();
    assert_eq!(choose_rate_for(44_100, &ACCEPTED, &s), Some(176_400));
    let s = parse_settings(&json!({"enabled": true, "max_ratio": 2})).unwrap();
    assert_eq!(choose_rate_for(48_000, &ACCEPTED, &s), Some(96_000));
    let s = parse_settings(&json!({"enabled": true})).unwrap();
    assert_eq!(choose_rate_for(44_100, &ACCEPTED, &s), Some(352_800));
    assert!(parse_settings(&json!({"enabled": true, "max_ratio": 3})).is_err());
}

#[test]
fn machine_pairs_cover_every_family_rate() {
    assert_eq!(machine_reference_pair(176_400), Some((44_100, 352_800)));
    assert_eq!(machine_reference_pair(705_600), Some((44_100, 705_600)));
    assert_eq!(machine_reference_pair(384_000), Some((48_000, 384_000)));
    assert_eq!(machine_reference_pair(768_000), Some((48_000, 768_000)));
    assert_eq!(machine_reference_pair(100_000), None);
}
