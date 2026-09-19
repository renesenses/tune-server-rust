use serde_json::json;
use std::collections::BTreeMap;
use tune_plugin_declick::{Declick, TrimOptions, trim_window};
use tune_plugin_sdk::{audio::*, batch::*};
use tune_plugin_testkit::batch::{MemoryHost, Source};
#[test]
fn stereo_right_channel_counts_and_pcm_metadata_are_preserved() {
    let mut h = MemoryHost::default();
    h.insert_track(
        1,
        Source {
            format: AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap(),
            samples: vec![0, 0, 0, 0, 0, 8000000, 0, -8000000, 0, 0, 0, 0],
            metadata: BTreeMap::from([("title".into(), "Original".into())]),
        },
    )
    .unwrap();
    let r = Declick
        .run(
            &mut h,
            &[SourceSelection::Track(1)],
            &json!({"output_format":"pcm-test","zero_cross":false}),
        )
        .unwrap();
    assert_eq!(r.state, JobState::Completed);
    let output = h.artifacts.values().next().unwrap();
    assert_eq!(output.samples, vec![0, 8000000, 0, -8000000]);
    assert_eq!(output.metadata, h.source(1).metadata);
}
#[test]
fn silent_track_failure_does_not_hide_successful_file() {
    let mut h = MemoryHost::default();
    for (id, samples) in [(1, vec![0; 64]), (2, vec![0, 30000, -30000, 0])] {
        h.insert_track(
            id,
            Source {
                format: AudioFormat::new(44100, ChannelLayout::Mono, SampleEncoding::S16).unwrap(),
                samples,
                metadata: BTreeMap::new(),
            },
        )
        .unwrap();
    }
    let r = Declick
        .run(
            &mut h,
            &[SourceSelection::Track(1), SourceSelection::Track(2)],
            &json!({"output_format":"pcm-test"}),
        )
        .unwrap();
    assert_eq!(r.state, JobState::Partial);
    assert_eq!(r.failures.len(), 1);
    assert_eq!(r.artifacts.len(), 1);
    assert_eq!(h.pending_count(), 0);
}
#[test]
fn zero_crossing_and_threshold_defaults_match_historical_semantics() {
    let s = [0, 1, 2, 30000, -30000, -2, -1, 0];
    assert_eq!(
        trim_window(&s, 1, 16, TrimOptions::default()).unwrap(),
        1..7
    );
    assert_eq!(
        trim_window(
            &s,
            1,
            16,
            TrimOptions {
                zero_cross: false,
                ..Default::default()
            }
        )
        .unwrap(),
        3..5
    );
    assert_eq!(
        trim_window(
            &s,
            1,
            16,
            TrimOptions {
                trim_lead: false,
                trim_tail: false,
                ..Default::default()
            }
        )
        .unwrap(),
        0..8
    );
}
