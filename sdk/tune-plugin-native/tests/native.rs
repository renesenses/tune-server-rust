use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tune_plugin_native::{Library, NativeBatch, stage::Stage};
use tune_plugin_sdk::{audio::*, batch::*};
use tune_plugin_testkit::batch::{MemoryHost, Source};
fn library(id: &str) -> Arc<Library> {
    // This suite is invoked by verify_native.py after building all four cdylibs.
    let directory = PathBuf::from(
        std::env::var_os("TUNE_NATIVE_TEST_DIR").expect("run sdk/scripts/verify_native.py first"),
    );
    let name = format!(
        "{}tune_plugin_{}{}",
        std::env::consts::DLL_PREFIX,
        id,
        std::env::consts::DLL_SUFFIX
    );
    unsafe { Library::load_trusted(&directory.join(name)) }.unwrap()
}
#[test]
fn native_dsp_processes_real_buffers_preserves_history_and_library_lifetime() {
    for (id, settings) in [
        (
            "equalizer",
            serde_json::json!({"enabled":true,"listening":"headphones","room_size":"medium","speaker_placement":"free_standing","bass_gain_db":0.0,"mid_gain_db":0.0,"treble_gain_db":0.0,"bands":[{"freq":1700.0,"gain":-7.0,"q":1.2,"channel":0}]}),
        ),
        (
            "crossfeed",
            serde_json::json!({"enabled":true,"amount":0.3,"delay_ms":0.3}),
        ),
    ] {
        let library = library(id);
        let mut a = Stage::prepare(library.clone(), 48000, 2, &settings).unwrap();
        let mut b = Stage::prepare(library.clone(), 48000, 2, &settings).unwrap();
        let input: Vec<f32> = (0..32768).map(|i| (i as f32 * 0.07).sin() * 0.8).collect();
        let mut whole = input.clone();
        a.process_f32(&mut whole).unwrap();
        let mut split = input.clone();
        for chunk in split.chunks_mut(254) {
            b.process_f32(chunk).unwrap();
        }
        assert_eq!(whole, split, "native block history {id}");
        assert_ne!(whole, input, "native DSP must actually run");
        let mut next = Stage::prepare(library.clone(), 48000, 2, &settings).unwrap();
        next.inherit(&b).unwrap();
        drop(b);
        drop(library);
        let mut x = input.clone();
        let mut y = input.clone();
        a.process_f32(&mut x).unwrap();
        next.process_f32(&mut y).unwrap();
        assert_eq!(x, y, "state handover {id}");
        for depth in [16, 24, 32] {
            let library = library_fn(id);
            let mut a = Stage::prepare(library.clone(), 44100, 2, &settings).unwrap();
            let mut b = Stage::prepare(library, 44100, 2, &settings).unwrap();
            let mut x = vec![0; 8192 * 2 * (depth / 8) as usize];
            for (i, v) in x.iter_mut().enumerate() {
                *v = (i % 251) as u8;
            }
            let mut y = x.clone();
            a.process_pcm(&mut x, depth).unwrap();
            for block in y.chunks_mut(2 * (depth / 8) as usize * 29) {
                b.process_pcm(block, depth).unwrap();
            }
            assert_eq!(x, y, "integer native depth {depth}");
        }
    }
}
fn library_fn(id: &str) -> Arc<Library> {
    library(id)
}
#[test]
fn native_batch_crosses_real_host_callbacks_with_cancellation_and_metadata() {
    for id in ["converter", "declick"] {
        let library = library(id);
        let tool = NativeBatch(library);
        let format = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap();
        let samples: Vec<i32> = (0..32768)
            .map(|i| {
                if !(200..32000).contains(&i) {
                    0
                } else {
                    if i % 2 == 0 { 12000 } else { -9000 }
                }
            })
            .collect();
        let mut host = MemoryHost::default();
        host.insert_track(
            1,
            Source {
                format,
                samples: samples.clone(),
                metadata: BTreeMap::from([("TITLE".into(), "Native fixture".into())]),
            },
        )
        .unwrap();
        let settings = if id == "converter" {
            serde_json::json!({"format":"pcm-test"})
        } else {
            serde_json::json!({"output_format":"pcm-test","zero_cross":false})
        };
        let result = tool
            .run(&mut host, &[SourceSelection::Track(1)], &settings)
            .unwrap();
        assert_eq!(result.state, JobState::Completed);
        let out = &host.artifacts[&result.artifacts[0].0];
        assert_eq!(out.metadata["TITLE"], "Native fixture");
        assert_eq!(
            out.samples,
            if id == "converter" {
                samples.clone()
            } else {
                samples[200..32000].to_vec()
            }
        );
        assert_eq!(host.source(1).samples, samples);
        let mut cancelled = MemoryHost::default();
        cancelled.insert_track(1, host.source(1).clone()).unwrap();
        cancelled.cancel_after_writes = Some(1);
        let result = tool
            .run(&mut cancelled, &[SourceSelection::Track(1)], &settings)
            .unwrap();
        assert_eq!(result.state, JobState::Cancelled);
        assert!(cancelled.artifacts.is_empty());
        assert_eq!(cancelled.pending_count(), 0);
    }
}

mod support;
#[test]
fn signed_native_activation_keeps_old_instance_alive_during_update_and_rollback() {
    use tune_plugin_native::package::*;
    let directory = PathBuf::from(std::env::var_os("TUNE_NATIVE_TEST_DIR").unwrap());
    let binary = directory.join(format!(
        "{}tune_plugin_crossfeed{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let manifest =
        serde_json::from_str(include_str!("../../tune-plugin-crossfeed/manifest.json")).unwrap();
    let first = pack(manifest, &binary, host_target(), &BTreeMap::new()).unwrap();
    let (keys, sig) = support::sign(&first);
    let temp = tempfile::tempdir().unwrap();
    install(temp.path(), &first, &sig, &keys).unwrap();
    let library = load(temp.path(), "crossfeed", &keys).unwrap();
    let settings = serde_json::json!({"enabled":true,"amount":0.3,"delay_ms":0.3});
    let mut processor = Stage::prepare(library.clone(), 48000, 2, &settings).unwrap();
    drop(library);
    let manifest =
        serde_json::from_str(include_str!("../../tune-plugin-crossfeed/manifest.json")).unwrap();
    let second = pack(
        manifest,
        &binary,
        host_target(),
        &BTreeMap::from([("ui/index.html".into(), b"updated signed UI".to_vec())]),
    )
    .unwrap();
    let (_, sig) = support::sign(&second);
    install(temp.path(), &second, &sig, &keys).unwrap();
    assert_eq!(
        read_asset(temp.path(), "crossfeed", "index.html", &keys).unwrap(),
        b"updated signed UI"
    );
    rollback(temp.path(), "crossfeed", &keys).unwrap();
    deactivate(temp.path(), "crossfeed").unwrap();
    let mut pcm = vec![0.0; 1024];
    pcm[0] = 0.8;
    processor.process_f32(&mut pcm).unwrap();
    assert!(
        pcm.iter().skip(1).any(|x| *x != 0.0),
        "old instance was lost during activation changes"
    );
    // Drop the processor before TempDir: Windows intentionally keeps its DLL mapped.
    drop(processor);
}
