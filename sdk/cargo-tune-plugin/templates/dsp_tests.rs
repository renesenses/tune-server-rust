use PLUGIN_CRATE::Plugin;
use tune_plugin_sdk::{Error, audio::*};
use tune_plugin_testkit::{assert_pcm_close, render_f32};

fn context() -> PlaybackContext {
    PlaybackContext {
        zone_id: 1,
        source: SourceKind::Library,
        delivery: Delivery::Local,
        pure: false,
        protected_bitstream: false,
    }
}
fn format() -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap()
}

#[test]
fn gain_reaches_captured_samples_across_block_sizes() {
    let input = [0.8, -0.6, 0.4, -0.2, 0.1, -0.8];
    for block_size in [1, 2, 3, 32] {
        let captured = render_f32(
            &Plugin,
            context(),
            true,
            &serde_json::json!({"gain": 0.5}),
            format(),
            &input,
            block_size,
        )
        .unwrap();
        assert_pcm_close(
            &captured.samples,
            &[0.4, -0.3, 0.2, -0.1, 0.05, -0.4],
            0.0,
            0.0,
        );
        assert!(captured.reports.iter().all(|r| r.changed));
    }
}

#[test]
fn bypass_preserves_original_bits() {
    let input = [-0.0, 0.9, -0.2, 0.3];
    for (pure, protected, licensed) in [
        (true, false, true),
        (false, true, true),
        (false, false, false),
    ] {
        let mut ctx = context();
        ctx.pure = pure;
        ctx.protected_bitstream = protected;
        let capture = render_f32(
            &Plugin,
            ctx,
            licensed,
            &serde_json::json!({"gain": 0.5}),
            format(),
            &input,
            1,
        )
        .unwrap();
        assert!(capture.reports.is_empty());
        assert_eq!(
            capture
                .samples
                .iter()
                .map(|s| s.to_bits())
                .collect::<Vec<_>>(),
            input.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn malformed_and_nonfinite_audio_is_rejected() {
    assert!(matches!(
        render_f32(
            &Plugin,
            context(),
            true,
            &serde_json::json!({"gain": 0.5}),
            format(),
            &[1.0],
            1
        ),
        Err(Error::IncompleteFrame)
    ));
    assert!(matches!(
        render_f32(
            &Plugin,
            context(),
            true,
            &serde_json::json!({"gain": 0.5}),
            format(),
            &[f32::NAN, 0.0],
            1
        ),
        Err(Error::NonFinite)
    ));
    assert!(
        Plugin
            .prepare(format(), 32, &serde_json::json!({"gain": 2.0}))
            .is_err()
    );
    let integer = AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::S32).unwrap();
    assert!(matches!(
        Plugin.prepare(integer, 32, &serde_json::json!({"gain": 0.5})),
        Err(Error::UnsupportedFormat)
    ));
}
