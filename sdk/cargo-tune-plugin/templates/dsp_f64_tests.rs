use PLUGIN_CRATE::Plugin;
use tune_plugin_sdk::{Error, audio::*};
use tune_plugin_testkit::{assert_pcm_close, render_f32, render_f64};

fn context() -> PlaybackContext {
    PlaybackContext {
        zone_id: 1,
        source: SourceKind::Library,
        delivery: Delivery::Local,
        pure: false,
        protected_bitstream: false,
    }
}
fn format(encoding: SampleEncoding) -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo, encoding).unwrap()
}

#[test]
fn manifest_requests_f64_without_requiring_it() {
    let manifest: tune_plugin_sdk::manifest::Manifest =
        serde_json::from_str(include_str!("../manifest.json")).unwrap();
    assert!(manifest.declares_f64());
    let f64_request = manifest
        .capabilities
        .iter()
        .find(|c| c.id == tune_plugin_sdk::manifest::AUDIO_F64)
        .unwrap();
    assert!(!f64_request.required, "an older host must still load it in F32");
}

#[test]
fn f64_blocks_keep_full_precision_across_block_sizes() {
    // Not representable in f32: a conversion anywhere would change the bits.
    let input = [0.123_456_789_012_345_6, -0.6, 0.4, -0.2, 1e-12, -0.8];
    for block_size in [1, 2, 3, 32] {
        let captured = render_f64(
            &Plugin,
            context(),
            true,
            &serde_json::json!({"gain": 0.5}),
            format(SampleEncoding::F64),
            &input,
            block_size,
        )
        .unwrap();
        let expected: Vec<u64> = input.iter().map(|v| (v * 0.5).to_bits()).collect();
        let actual: Vec<u64> = captured.samples.iter().map(|v| v.to_bits()).collect();
        assert_eq!(actual, expected, "f64 lost precision (block {block_size})");
        assert!(captured.reports.iter().all(|r| r.changed));
    }
}

#[test]
fn f32_blocks_still_work_for_older_hosts() {
    let captured = render_f32(
        &Plugin,
        context(),
        true,
        &serde_json::json!({"gain": 0.5}),
        format(SampleEncoding::F32),
        &[0.8, -0.6, 0.4, -0.2],
        2,
    )
    .unwrap();
    assert_pcm_close(&captured.samples, &[0.4, -0.3, 0.2, -0.1], 0.0, 0.0);
}

#[test]
fn malformed_and_nonfinite_audio_is_rejected() {
    assert!(matches!(
        render_f64(
            &Plugin,
            context(),
            true,
            &serde_json::json!({"gain": 0.5}),
            format(SampleEncoding::F64),
            &[1.0],
            1
        ),
        Err(Error::IncompleteFrame)
    ));
    assert!(matches!(
        render_f64(
            &Plugin,
            context(),
            true,
            &serde_json::json!({"gain": 0.5}),
            format(SampleEncoding::F64),
            &[f64::NAN, 0.0],
            1
        ),
        Err(Error::NonFinite)
    ));
    assert!(
        Plugin
            .prepare(
                format(SampleEncoding::F64),
                32,
                &serde_json::json!({"gain": 2.0})
            )
            .is_err()
    );
}
