use serde_json::json;
use tune_plugin_crossfeed::{Crossfeed, CrossfeedProcessor};
use tune_plugin_sdk::audio::*;
fn process(p: &mut dyn Processor, f: AudioFormat, samples: &mut [f32]) {
    p.process(
        &mut AudioBlock::new(f, SamplesMut::F32(samples), 4096).unwrap(),
        BlockContext {
            zone_id: 1,
            generation: 1,
            position_frames: 0,
        },
    )
    .unwrap();
}
#[test]
fn sdk_crossfeed_preserves_delay_across_blocks_and_live_update() {
    let f = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let mut p = Crossfeed
        .prepare(
            f,
            4096,
            &json!({"enabled":true,"amount":0.3,"delay_ms":0.3}),
        )
        .unwrap();
    let signal: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.137).sin() * 0.4).collect();
    let mut actual = signal.clone();
    process(&mut *p, f, &mut actual[..512]);
    p.update(&json!({"enabled":true,"amount":0.4,"delay_ms":0.3}))
        .unwrap();
    for chunk in actual[512..].chunks_mut(128) {
        process(&mut *p, f, chunk);
    }
    let mut old = CrossfeedProcessor::new(48000, 0.3, 0.3);
    let mut expected = signal;
    old.process_interleaved(&mut expected[..512]);
    let mut next = CrossfeedProcessor::new(48000, 0.4, 0.3);
    next.inherit_state_from(&old);
    next.process_interleaved(&mut expected[512..]);
    assert_eq!(
        actual, expected,
        "SDK lost the delay history or skipped processing"
    );
}
#[test]
fn protected_and_pure_bypass_and_multichannel_refusal() {
    let settings = json!({"enabled":true});
    let mut ctx = PlaybackContext {
        zone_id: 1,
        source: SourceKind::Streaming,
        delivery: Delivery::NetworkFile,
        pure: true,
        protected_bitstream: false,
    };
    assert_eq!(
        Crossfeed.assess(&ctx, &settings).unwrap(),
        Applicability::Bypass(BypassReason::Pure)
    );
    ctx.pure = false;
    ctx.protected_bitstream = true;
    assert_eq!(
        Crossfeed.assess(&ctx, &settings).unwrap(),
        Applicability::Bypass(BypassReason::ProtectedBitstream)
    );
    let f = AudioFormat::new(48000, ChannelLayout::Discrete(6), SampleEncoding::F32).unwrap();
    assert!(Crossfeed.prepare(f, 512, &settings).is_err());
}
#[test]
fn integer_sdk_matches_legacy_quantization() {
    let f = AudioFormat::new(96000, ChannelLayout::Stereo, SampleEncoding::S32).unwrap();
    let mut p = Crossfeed
        .prepare(f, 256, &json!({"enabled":true,"amount":0.3,"delay_ms":0.3}))
        .unwrap();
    let mut actual: Vec<i32> = (0..512)
        .map(|i| ((i as f64 * 0.071).sin() * 1e9) as i32)
        .collect();
    let mut expected: Vec<u8> = actual.iter().flat_map(|x| x.to_le_bytes()).collect();
    CrossfeedProcessor::new(96000, 0.3, 0.3).process_pcm(&mut expected, 32, 2);
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S32(&mut actual), 256).unwrap(),
        BlockContext {
            zone_id: 1,
            generation: 1,
            position_frames: 0,
        },
    )
    .unwrap();
    assert_eq!(
        actual
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect::<Vec<_>>(),
        expected
    );
}
