//! #7614 — the upsampler through the SDK's rate-changing stage contract.
use serde_json::json;
use tune_plugin_sdk::{Error, Settings, audio::*};
use tune_plugin_surechantillonnage::conception::par_id;
use tune_plugin_surechantillonnage::sdk::{Upsampler, choose_rate};
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
                let want = 0.891 * (std::f64::consts::TAU * f * t).sin();
                let got = f64::from(cap.samples[2 * k + ch]);
                err += (got - want).powi(2);
                sig += want * want;
            }
        }
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
