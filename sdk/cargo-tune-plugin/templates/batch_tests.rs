use PLUGIN_CRATE::Plugin;
use tune_plugin_sdk::{audio::*, batch::*};
use tune_plugin_testkit::batch::{MemoryHost, Source};

fn host() -> MemoryHost {
    let mut host = MemoryHost::default();
    host.insert_track(
        7,
        Source {
            format: AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::S32).unwrap(),
            samples: (0..10_000).map(|i| i * 997 - 123).collect(),
            metadata: [("artist".into(), "Reference Artist".into())].into(),
        },
    )
    .unwrap();
    host
}

#[test]
fn batch_preserves_pcm_metadata_and_source() {
    let mut host = host();
    let original = host.source(7).clone();
    let result = Plugin
        .run(
            &mut host,
            &[SourceSelection::Track(7)],
            &serde_json::json!({"codec": "pcm-test"}),
        )
        .unwrap();
    assert_eq!(result.state, JobState::Completed);
    assert_eq!(result.artifacts.len(), 1);
    let output = &host.artifacts[&result.artifacts[0].0];
    assert_eq!(
        output.samples, original.samples,
        "batch PCM must reach the finished artifact unchanged"
    );
    assert_eq!(
        output.metadata, original.metadata,
        "metadata must reach the finished artifact"
    );
    assert_eq!(
        host.source(7).samples,
        original.samples,
        "source must remain unchanged"
    );
    assert_eq!(host.progress.last().map(|p| (p.0, p.1)), Some((1, 1)));
    assert_eq!(host.pending_count(), 0);
}

#[test]
fn cancellation_inside_a_file_never_publishes_partial_output() {
    let mut host = host();
    host.cancel_after_writes = Some(1);
    let result = Plugin
        .run(
            &mut host,
            &[SourceSelection::Track(7)],
            &serde_json::json!({"codec": "pcm-test"}),
        )
        .unwrap();
    assert_eq!(result.state, JobState::Cancelled);
    assert!(host.artifacts.is_empty());
    assert_eq!(
        host.pending_count(),
        0,
        "cancelled job must abort its unpublished writer"
    );
    assert_eq!(host.source(7).samples.len(), 10_000);
}

#[test]
fn unavailable_codec_is_an_explicit_file_failure() {
    let mut host = host();
    let result = Plugin
        .run(
            &mut host,
            &[SourceSelection::Track(7)],
            &serde_json::json!({"codec": "missing"}),
        )
        .unwrap();
    assert_eq!(result.state, JobState::Failed);
    assert_eq!(result.failures[0].code, "CapabilityMissing");
    assert!(host.artifacts.is_empty());
}
