use serde_json::json;
use std::collections::BTreeMap;
use tune_plugin_converter::Converter;
use tune_plugin_sdk::{audio::*, batch::*};
use tune_plugin_testkit::batch::{MemoryHost, Source};
fn host() -> MemoryHost {
    let mut h = MemoryHost::default();
    h.insert_track(
        1,
        Source {
            format: AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::S32).unwrap(),
            samples: (0..32768).map(|i| 16777217 + i).collect(),
            metadata: BTreeMap::from([("artist".into(), "Témoin".into())]),
        },
    )
    .unwrap();
    h
}
#[test]
fn converter_preserves_integer_precision_and_metadata() {
    let mut h = host();
    let r = Converter
        .run(
            &mut h,
            &[SourceSelection::Track(1)],
            &json!({"format":"pcm-test"}),
        )
        .unwrap();
    assert_eq!(r.state, JobState::Completed);
    let output = h.artifacts.values().next().unwrap();
    assert_eq!(output.samples, h.source(1).samples);
    assert_eq!(output.metadata, h.source(1).metadata);
    assert_eq!(h.pending_count(), 0);
}
#[test]
fn converter_cancels_inside_first_file_without_publishing() {
    let mut h = host();
    h.cancel_after_writes = Some(1);
    let r = Converter
        .run(
            &mut h,
            &[SourceSelection::Track(1)],
            &json!({"format":"pcm-test"}),
        )
        .unwrap();
    assert_eq!(r.state, JobState::Cancelled);
    assert!(h.artifacts.is_empty());
    assert_eq!(h.pending_count(), 0);
}
#[test]
fn unsupported_conversion_is_an_explicit_failure() {
    let mut h = host();
    let r = Converter
        .run(
            &mut h,
            &[SourceSelection::Track(1)],
            &json!({"format":"pcm-test","sample_rate":96000}),
        )
        .unwrap();
    assert_eq!(r.state, JobState::Failed);
    assert!(h.artifacts.is_empty());
    assert_eq!(h.pending_count(), 0);
}
