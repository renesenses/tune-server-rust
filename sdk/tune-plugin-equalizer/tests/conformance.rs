use tune_plugin_equalizer::{EqBandSpec, EqProcessor, EqProfile, Equalizer};
use tune_plugin_sdk::audio::*;
fn profile() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![EqBandSpec {
            freq: 1700.0,
            gain: -7.0,
            q: 1.2,
            channel: Some(0),
            ..Default::default()
        }],
        ..Default::default()
    }
}
#[test]
fn sdk_equalizer_preserves_channel_filters_headroom_and_live_state() {
    let f = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let settings = serde_json::to_value(profile()).unwrap();
    let mut sdk = Equalizer.prepare(f, 512, &settings).unwrap();
    let input: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.07).sin() * 0.4).collect();
    let mut expected = input.clone();
    let mut engine = EqProcessor::new(&profile(), 48000, 2);
    engine.process_interleaved(&mut expected);
    let mut actual = input.clone();
    for chunk in actual.chunks_mut(512) {
        sdk.process(
            &mut AudioBlock::new(f, SamplesMut::F32(chunk), 512).unwrap(),
            BlockContext {
                zone_id: 1,
                generation: 2,
                position_frames: 0,
            },
        )
        .unwrap();
        sdk.update(&settings).unwrap();
    }
    assert_eq!(
        actual, expected,
        "SDK EQ processing/state differs from legacy processor"
    );
    assert_ne!(actual[0..100], input[0..100]);
    for (a, b) in actual
        .iter()
        .skip(1)
        .step_by(2)
        .zip(input.iter().skip(1).step_by(2))
    {
        assert_eq!(a, b, "right channel was modified by a left-only band");
    }
}
#[test]
fn integer_sdk_preserves_dither_sequence_between_blocks() {
    let f = AudioFormat::new(44100, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap();
    let mut sdk = Equalizer
        .prepare(f, 32, &serde_json::to_value(profile()).unwrap())
        .unwrap();
    let mut actual = vec![0_u8; 6 * 128];
    let mut expected = actual.clone();
    EqProcessor::new(&profile(), 44100, 2).process_pcm(&mut expected, 24);
    for chunk in actual.chunks_mut(6 * 32) {
        sdk.process(
            &mut AudioBlock::new(f, SamplesMut::S24Le(chunk), 32).unwrap(),
            BlockContext {
                zone_id: 1,
                generation: 1,
                position_frames: 0,
            },
        )
        .unwrap();
    }
    assert_eq!(
        actual, expected,
        "dither sequence restarted at a block boundary"
    );
}
#[test]
fn response_uses_prepared_coefficients_and_matches_steady_sine() {
    let profile = profile();
    let mut processor = EqProcessor::new(&profile, 48000, 2);
    let curve = processor.response(48000);
    let index = 150;
    let frequency = curve["frequency_hz"][index].as_f64().unwrap();
    let mut samples: Vec<f32> = (0..48000)
        .flat_map(|i| {
            let x =
                (2.0 * std::f64::consts::PI * frequency * i as f64 / 48000.0).sin() as f32 * 0.01;
            [x, x]
        })
        .collect();
    let reference = samples.clone();
    processor.process_interleaved(&mut samples);
    let energy = |x: &[f32]| {
        x[48000..]
            .iter()
            .step_by(2)
            .map(|x| f64::from(*x).powi(2))
            .sum::<f64>()
    };
    let measured = 10.0 * (energy(&samples) / energy(&reference)).log10();
    assert!(
        (measured - curve["channels_db"][0][index].as_f64().unwrap()).abs() < 0.01,
        "curve differs from the actual filter"
    );
    assert_eq!(curve["channels_db"][1][index], 0.0);
}
