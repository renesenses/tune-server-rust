//! TESS T13 — the SDK contract of `declick-live`.
mod common;
use common::*;
use serde_json::json;
use tune_plugin_declick_live::corpus::{programme, stereo};
use tune_plugin_declick_live::{DeclickLiveFactory, MANIFEST};
use tune_plugin_sdk::Error;
use tune_plugin_sdk::audio::*;
use tune_plugin_sdk::manifest::{Manifest, PluginKind};

#[test]
fn manifest_is_a_premium_dsp_distinct_from_the_batch_tool() {
    let m: Manifest = serde_json::from_str(MANIFEST).unwrap();
    m.validate().unwrap();
    assert_eq!(m.id, "declick-live");
    assert_ne!(m.id, "declick", "the batch tool keeps its own id");
    assert_eq!(m.kind, PluginKind::Dsp);
    // Decision P1 (17/11/2026): Premium, through the existing entitlement.
    assert_eq!(m.entitlement, "declick");
    assert_ne!(m.entitlement, "free");
}

fn playback(pure: bool, protected: bool) -> PlaybackContext {
    PlaybackContext {
        zone_id: 1,
        source: SourceKind::Library,
        delivery: Delivery::Local,
        pure,
        protected_bitstream: protected,
    }
}

#[test]
fn disabled_by_default_and_host_policy_bypasses() {
    let f = DeclickLiveFactory;
    for s in [
        serde_json::Value::Null,
        json!({}),
        json!({"enabled": false}),
    ] {
        assert_eq!(
            f.assess(&playback(false, false), &s).unwrap(),
            Applicability::Bypass(BypassReason::Disabled)
        );
    }
    assert_eq!(
        f.assess(&playback(true, false), &enabled()).unwrap(),
        Applicability::Bypass(BypassReason::Pure)
    );
    assert_eq!(
        f.assess(&playback(false, true), &enabled()).unwrap(),
        Applicability::Bypass(BypassReason::ProtectedBitstream)
    );
    assert_eq!(
        f.assess(&playback(false, false), &enabled()).unwrap(),
        Applicability::Process { requires_pcm: true }
    );
}

#[test]
fn settings_are_bounded() {
    let f = DeclickLiveFactory;
    let fmt = AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    for bad in [
        json!({"enabled": true, "lookahead_ms": 9.9}),
        json!({"enabled": true, "lookahead_ms": 20.1}),
        json!({"enabled": true, "threshold": 3.0}),
        json!({"enabled": true, "threshold": 31.0}),
        json!({"enabled": true, "max_click_ms": 2.5}),
        json!({"enabled": "yes"}),
    ] {
        assert!(
            matches!(f.prepare(fmt, 512, &bad), Err(Error::InvalidSettings)),
            "{bad}"
        );
    }
    // Below 32 kHz the lookahead cannot hold the context the method needs.
    let low = AudioFormat::new(22_050, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    assert!(matches!(
        f.prepare(low, 512, &enabled()),
        Err(Error::UnsupportedFormat)
    ));
}

#[test]
fn latency_is_the_announced_lookahead_between_10_and_20_ms() {
    for rate in [32_000, 44_100, 48_000, 96_000, 192_000, 384_000, 768_000] {
        for ms in [10.0, 15.0, 20.0] {
            let fmt = AudioFormat::new(rate, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap();
            let p = DeclickLiveFactory
                .prepare(fmt, 1024, &json!({"enabled": true, "lookahead_ms": ms}))
                .unwrap_or_else(|e| panic!("{rate} Hz, {ms} ms: {e:?}"));
            let expected = (ms * f64::from(rate) / 1000.0).round() as u32;
            assert_eq!(p.latency_frames(), expected);
        }
    }
}

#[test]
fn every_encoding_round_trips_bit_exact_through_the_delay_line() {
    let x = stereo(programme, 48_000, 3.0, 11);
    for enc in [
        SampleEncoding::S16,
        SampleEncoding::S24Le,
        SampleEncoding::S32,
        SampleEncoding::F32,
        SampleEncoding::F64,
    ] {
        let input = Pcm::quantize(enc, &x);
        let r = run(48_000, 2, &input, &enabled());
        assert!(r.events.is_empty(), "{enc:?}: {:?}", r.events);
        assert!(
            r.output == input,
            "{enc:?}: output is not the delayed input"
        );
    }
}

#[test]
fn live_update_keeps_latency_and_refuses_a_new_lookahead() {
    let fmt = AudioFormat::new(44_100, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let mut p = DeclickLiveFactory.prepare(fmt, 512, &enabled()).unwrap();
    let before = p.latency_frames();
    p.update(&json!({"enabled": true, "threshold": 10.0, "max_click_ms": 1.0}))
        .unwrap();
    assert_eq!(p.latency_frames(), before);
    assert_eq!(
        p.update(&json!({"enabled": true, "lookahead_ms": 20.0})),
        Err(Error::InvalidState)
    );
    assert_eq!(
        p.update(&json!({"threshold": 50.0})),
        Err(Error::InvalidSettings)
    );
}

#[test]
fn inheriting_a_processor_continues_the_stream_exactly() {
    let rate = 44_100;
    let x = stereo(programme, rate, 2.0, 5);
    let fmt = AudioFormat::new(rate, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let input: Vec<f32> = x.iter().map(|v| *v as f32).collect();
    let mut one = input.clone();
    let mut p = DeclickLiveFactory.prepare(fmt, 512, &enabled()).unwrap();
    for c in one.chunks_mut(1024) {
        p.process(
            &mut AudioBlock::new(fmt, SamplesMut::F32(c), 512).unwrap(),
            ctx(),
        )
        .unwrap();
    }
    let mut two = input;
    let half = two.len() / 2 / 1024 * 1024;
    let mut a = DeclickLiveFactory.prepare(fmt, 512, &enabled()).unwrap();
    for c in two[..half].chunks_mut(1024) {
        a.process(
            &mut AudioBlock::new(fmt, SamplesMut::F32(c), 512).unwrap(),
            ctx(),
        )
        .unwrap();
    }
    let mut b = DeclickLiveFactory.prepare(fmt, 512, &enabled()).unwrap();
    b.inherit_from(&*a).unwrap();
    for c in two[half..].chunks_mut(1024) {
        b.process(
            &mut AudioBlock::new(fmt, SamplesMut::F32(c), 512).unwrap(),
            ctx(),
        )
        .unwrap();
    }
    assert!(one == two);
}

#[test]
fn reset_starts_a_new_stream() {
    let fmt = AudioFormat::new(48_000, ChannelLayout::Mono, SampleEncoding::F32).unwrap();
    let mut p = DeclickLiveFactory.prepare(fmt, 2048, &enabled()).unwrap();
    let mut x = vec![0.5f32; 2048];
    p.process(
        &mut AudioBlock::new(fmt, SamplesMut::F32(&mut x), 2048).unwrap(),
        ctx(),
    )
    .unwrap();
    p.reset(ResetReason::Seek);
    let mut y = vec![0.25f32; 2048];
    p.process(
        &mut AudioBlock::new(fmt, SamplesMut::F32(&mut y), 2048).unwrap(),
        ctx(),
    )
    .unwrap();
    let l = p.latency_frames() as usize;
    assert!(
        y[..l].iter().all(|v| *v == 0.0),
        "no sample of the old stream after reset"
    );
    assert!(y[l..].iter().all(|v| *v == 0.25));
    let mut nan = vec![f32::NAN; 4];
    assert_eq!(
        p.process(
            &mut AudioBlock::new(fmt, SamplesMut::F32(&mut nan), 2048).unwrap(),
            ctx()
        ),
        Err(Error::NonFinite)
    );
}
