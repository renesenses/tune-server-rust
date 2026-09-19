use std::collections::BTreeMap;
use tune_plugin_sdk::{Error, audio::*, batch::*, manifest::*, observation::*, ui::*};

fn format(encoding: SampleEncoding) -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo, encoding).unwrap()
}

#[test]
fn no_invalid_format_or_partial_frame_reaches_a_processor() {
    assert_eq!(
        AudioFormat::new(0, ChannelLayout::Stereo, SampleEncoding::F32),
        Err(Error::InvalidFormat)
    );
    assert_eq!(
        AudioFormat::new(48_000, ChannelLayout::Discrete(0), SampleEncoding::S32),
        Err(Error::InvalidFormat)
    );
    let mut bytes = [0u8; 5];
    assert!(matches!(
        AudioBlock::new(
            format(SampleEncoding::S24Le),
            SamplesMut::S24Le(&mut bytes),
            8
        ),
        Err(Error::IncompleteFrame)
    ));
    let mut integers = [0i32; 4];
    assert!(matches!(
        AudioBlock::new(
            format(SampleEncoding::F32),
            SamplesMut::S32(&mut integers),
            8
        ),
        Err(Error::InvalidFormat)
    ));
    assert!(matches!(
        AudioBlock::new(
            format(SampleEncoding::S32),
            SamplesMut::S32(&mut integers),
            1
        ),
        Err(Error::BlockTooLarge)
    ));
    assert_eq!(
        AudioBlock::new(
            format(SampleEncoding::S32),
            SamplesMut::S32(&mut integers),
            2
        )
        .unwrap()
        .frames(),
        2
    );
}

#[test]
fn integer_bypass_requires_no_float_conversion() {
    let original = [i32::MAX, i32::MIN, 16_777_217, -16_777_217];
    let mut pcm = original;
    let block = AudioBlock::new(format(SampleEncoding::S32), SamplesMut::S32(&mut pcm), 2).unwrap();
    assert_eq!(block.format().encoding(), SampleEncoding::S32);
    assert_eq!(
        pcm, original,
        "constructing a PCM block must not round through f32"
    );
}

fn manifest() -> Manifest {
    serde_json::from_value(serde_json::json!({
        "id": "example", "sdk": {"major": 0, "minor": 1}, "kind": "dsp",
        "config_version": 1, "entitlement": "plugin.example", "distribution": "source",
        "capabilities": [
            {"id": "audio-process", "version": {"major": 0, "minor": 1}, "required": true},
            {"id": "audio-observe", "version": {"major": 0, "minor": 1}, "required": false}
        ]
    }))
    .unwrap()
}

#[test]
fn missing_required_capability_is_rejected_before_setup() {
    assert_eq!(
        manifest().negotiate(&BTreeMap::new()),
        Err(ManifestError::MissingCapability("audio-process".into()))
    );
    let host = [
        ("audio-process".into(), SDK_VERSION),
        ("file-jobs".into(), SDK_VERSION),
    ]
    .into();
    assert_eq!(
        manifest().negotiate(&host).unwrap(),
        ["audio-process".into()].into()
    );
}

#[test]
fn experimental_minor_versions_are_not_assumed_binary_compatible() {
    assert!(!Version { major: 0, minor: 2 }.supports(SDK_VERSION));
    assert!(Version { major: 1, minor: 2 }.supports(Version { major: 1, minor: 1 }));
    let mut m = manifest();
    m.sdk.minor = 2;
    assert_eq!(m.validate(), Err(ManifestError::UnsupportedSdk));
    let mut m = manifest();
    m.distribution = "native".into();
    assert_eq!(m.validate(), Err(ManifestError::UnsupportedDistribution));
    let mut m = manifest();
    m.capabilities.push(m.capabilities[0].clone());
    assert!(matches!(
        m.validate(),
        Err(ManifestError::DuplicateCapability(_))
    ));
}

fn spectrum() -> SpectrumFrame {
    SpectrumFrame {
        stamp: ObservationStamp {
            zone_id: 7,
            generation: 5,
            position_frames: 0,
            monotonic_ns: 0,
            format: format(SampleEncoding::S32),
            point: ObservationPoint::DecodedSource,
            provenance: Provenance::Pipeline,
            dropped_frames: 0,
        },
        relative: vec![0.5, 1.0],
        dbfs: vec![-20.0, -10.0],
        frequencies_hz: vec![100.0, 1000.0],
        resolved: vec![true, true],
        fft_size: 2048,
        frames_analyzed: 1920,
        resolution_hz: 25.0,
    }
}

#[test]
fn spectrum_requires_real_axes_resolution_and_measurement_provenance() {
    assert_eq!(spectrum().validate(), Ok(()));
    let mut s = spectrum();
    s.dbfs.clear();
    assert_eq!(
        s.validate(),
        Err(Error::InvalidObservation),
        "missing spectrum data must fail"
    );
    let mut s = spectrum();
    s.resolution_hz = 48_000.0 / 2048.0;
    assert_eq!(
        s.validate(),
        Err(Error::InvalidObservation),
        "padding is not actual signal resolution"
    );
    let mut s = spectrum();
    s.frequencies_hz[0] = f32::NAN;
    assert_eq!(s.validate(), Err(Error::InvalidObservation));
    let mut s = spectrum();
    s.stamp.provenance = Provenance::SourceProbe;
    s.stamp.point = ObservationPoint::PostDsp;
    assert_eq!(
        s.validate(),
        Err(Error::InvalidObservation),
        "a source probe cannot measure post-DSP audio"
    );
}

#[test]
fn old_track_or_other_zone_observations_are_not_displayed() {
    let mut cursor = ObservationCursor::new(7, 5);
    let mut stamp = spectrum().stamp;
    stamp.position_frames = 500;
    assert!(cursor.accepts(&stamp));
    stamp.generation = 4;
    assert!(
        !cursor.accepts(&stamp),
        "previous track must not repopulate the spectrum"
    );
    stamp.generation = 5;
    stamp.zone_id = 8;
    assert!(!cursor.accepts(&stamp));
    stamp.zone_id = 7;
    stamp.position_frames = 499;
    assert!(!cursor.accepts(&stamp));
    assert!(ObservationCursor::new(7, 5).accepts(&stamp));
}

#[test]
fn codec_support_includes_quality_and_format_not_just_its_name() {
    let c = CodecCapability {
        codec: "test".into(),
        sample_rates: vec![48_000],
        bit_depths: vec![24],
        qualities: vec!["high".into()],
    };
    let mut o = EncodeOptions {
        codec: "test".into(),
        sample_rate: 48_000,
        bit_depth: 24,
        quality: Some("high".into()),
    };
    assert!(c.supports(&o));
    o.quality = Some("unknown".into());
    assert!(!c.supports(&o));
    o.quality = None;
    o.sample_rate = 96_000;
    assert!(!c.supports(&o));
}

#[test]
fn ui_messages_are_versioned_scoped_and_reject_unknown_commands() {
    let ctx = UiContext {
        protocol: SDK_VERSION,
        plugin_id: "eq".into(),
        zone_id: Some(7),
        locale: "fr".into(),
        theme: "dark".into(),
    };
    let mut req = UiRequest {
        protocol: SDK_VERSION,
        request_id: 9,
        plugin_id: "eq".into(),
        action: UiCommand::GetConfiguration,
    };
    assert!(req.matches_session(&ctx));
    req.plugin_id = "converter".into();
    assert!(!req.matches_session(&ctx));
    assert!(
        serde_json::from_value::<UiCommand>(
            serde_json::json!({"command": "shell", "cmd": "anything"})
        )
        .is_err()
    );
}
