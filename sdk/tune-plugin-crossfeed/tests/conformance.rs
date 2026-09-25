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

/// #4973 — les rails et la demi-échelle, par PROFONDEUR : une source MONO
/// (L = R) traverse le crossfeed actif sans perdre un bit, par l'instance du
/// greffon comme par `process_pcm`. Avant, l'encodage à 2^(N−1) − 1 rendait
/// −32 768 en −32 767, et 32 767 en 32 766.
#[test]
fn une_source_mono_traverse_le_greffon_au_bit_pres_4973() {
    let reglage = json!({"enabled":true,"amount":0.3,"delay_ms":0.3});
    let contexte = || BlockContext {
        zone_id: 1,
        generation: 1,
        position_frames: 0,
    };
    let stereo = |mots: &[i32]| -> Vec<i32> { mots.iter().flat_map(|m| [*m, *m]).collect() };

    // 16 bits.
    let mots = stereo(&[
        -32_768, -32_767, -16_385, -1, 0, 1, 16_384, 16_385, 32_766, 32_767,
    ]);
    let entree: Vec<i16> = mots.iter().map(|m| *m as i16).collect();
    let f = AudioFormat::new(44_100, ChannelLayout::Stereo, SampleEncoding::S16).unwrap();
    let mut p = Crossfeed.prepare(f, 64, &reglage).unwrap();
    let mut sortie = entree.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S16(&mut sortie), 64).unwrap(),
        contexte(),
    )
    .unwrap();
    assert_eq!(sortie, entree, "16 bits");

    // 24 bits, petit-boutien compact.
    let mots = stereo(&[
        -8_388_608, -8_388_607, -4_194_305, -1, 0, 1, 4_194_304, 8_388_606, 8_388_607,
    ]);
    let entree: Vec<u8> = mots
        .iter()
        .flat_map(|m| {
            let b = m.to_le_bytes();
            [b[0], b[1], b[2]]
        })
        .collect();
    let f = AudioFormat::new(96_000, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap();
    let mut p = Crossfeed.prepare(f, 64, &reglage).unwrap();
    let mut sortie = entree.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S24Le(&mut sortie), 64).unwrap(),
        contexte(),
    )
    .unwrap();
    assert_eq!(sortie, entree, "24 bits");

    // 32 bits : les rails, et un 24 bits aligné à gauche — ce qu'un `f32`
    // porte exactement.
    let entree = stereo(&[
        i32::MIN,
        -8_388_607 << 8,
        -1,
        0,
        1,
        8_388_607 << 8,
        i32::MAX,
    ]);
    let f = AudioFormat::new(192_000, ChannelLayout::Stereo, SampleEncoding::S32).unwrap();
    let mut p = Crossfeed.prepare(f, 64, &reglage).unwrap();
    let mut sortie = entree.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S32(&mut sortie), 64).unwrap(),
        contexte(),
    )
    .unwrap();
    assert_eq!(sortie, entree, "32 bits");
}
