use super::*;
use crate::orchestrator::tests::test_orchestrator;
use crate::streaming::StreamTrack;
use crate::streaming::audio_source::PcmSender;

fn fixture(format: PcmFormat) -> (PcmSender, DecodedPcmSource) {
    let track: StreamTrack = serde_json::from_value(serde_json::json!({
        "id": "independent-source", "title": "PCM witness", "artist": "Fixture",
        "duration_ms": 999999, "explicit": false
    }))
    .unwrap();
    DecodedPcmSource::channel(format, AudioFormat::Flac, track).unwrap()
}

fn cd() -> PcmFormat {
    PcmFormat {
        sample_rate: 44100,
        bit_depth: 16,
        channels: 2,
    }
}

#[tokio::test]
async fn decoded_pcm_late_source_never_attaches_to_new_play_generation() {
    let orch = test_orchestrator();
    let (tx, source) = fixture(cd());
    let old_generation = orch.playback.current_play_seq(1).await;
    orch.playback.bump_generation(1).await;
    let error = orch
        .serve_decoded_pcm(
            "late-provider",
            source,
            old_generation,
            &PlayRequest {
                zone_id: 1,
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert!(
        error.contains("superseded"),
        "late PCM must not claim newer playback"
    );
    tokio::time::timeout(Duration::from_millis(100), tx.closed())
        .await
        .unwrap();
    assert!(orch.streamer.sessions_state().lock().await.is_empty());
}

#[tokio::test]
async fn decoded_pcm_capability_routes_any_provider_to_common_session_and_spectrum() {
    let mut orch = test_orchestrator();
    let bus = Arc::new(EventBus::new());
    let mut events = bus.subscribe();
    orch.event_bus = Some(bus);
    orch.services
        .lock()
        .await
        .register(Box::new(crate::streaming::test_service::TestService(
            "fixture-decoded",
        )));
    orch.playback.play(1, NowPlaying::default()).await;
    let resolved = orch
        .resolve_streaming_url(
            "fixture-decoded",
            &PlayRequest {
                zone_id: 1,
                source: Some("fixture-decoded".into()),
                source_id: Some("pcm".into()),
                ..Default::default()
            },
        )
        .await
        .expect("decoded capability must use common host adapter, never get_track_url");
    let levels = next_level(&mut events).await;
    assert!(levels["rms_left"].as_f64().unwrap() > 0.0);
    assert_eq!(resolved.title, "Generic provider");
    orch.streamer
        .remove_session(&resolved.stream_id.unwrap())
        .await;
}

#[tokio::test]
async fn decoded_pcm_refuses_unapplied_dsp_before_opening_provider() {
    let orch = test_orchestrator();
    orch.services
        .lock()
        .await
        .register(Box::new(crate::streaming::test_service::TestService(
            "fixture-private",
        )));
    let profile = crate::audio::eq::EqProfile {
        enabled: true,
        bass_gain_db: 6.0,
        ..Default::default()
    };
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            "zone_1_eq_profile",
            &serde_json::to_string(&profile).unwrap(),
        )
        .unwrap();
    let error = orch
        .resolve_streaming_url(
            "fixture-private",
            &PlayRequest {
                zone_id: 1,
                source_id: Some("pcm".into()),
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert!(
        error.contains("does not yet apply zone DSP"),
        "must refuse DSP before opening decoder: {error}"
    );
    assert!(orch.streamer.sessions_state().lock().await.is_empty());
}

#[tokio::test]
async fn decoded_pcm_analysis_freezes_on_pause_resumes_and_stops_with_playback() {
    let mut orch = test_orchestrator();
    let bus = Arc::new(EventBus::new());
    let mut events = bus.subscribe();
    orch.event_bus = Some(bus);
    orch.playback.play(1, NowPlaying::default()).await;
    let (tx, source) = fixture(cd());
    tokio::spawn(async move {
        for chunk in signal(cd(), 50).chunks(16000) {
            tx.send(chunk.to_vec()).await.unwrap();
        }
    });
    let result = orch
        .serve_decoded_pcm(
            "fixture",
            source,
            0,
            &PlayRequest {
                zone_id: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let first = next_level(&mut events).await;
    assert_eq!(first["position_ms"], 0);
    orch.playback.pause(1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    while events.try_recv().is_ok() {}
    assert!(
        tokio::time::timeout(Duration::from_millis(250), events.recv())
            .await
            .is_err(),
        "paused PCM must not animate spectrum"
    );
    orch.playback.resume(1).await;
    let resumed = next_level(&mut events).await;
    assert!(
        resumed["position_ms"].as_u64().unwrap() <= 120,
        "analysis clock must freeze instead of skipping ahead during pause"
    );
    orch.playback.stop(1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    while events.try_recv().is_ok() {}
    assert!(
        tokio::time::timeout(Duration::from_millis(250), events.recv())
            .await
            .is_err(),
        "stopped PCM must stop spectrum events"
    );
    orch.streamer
        .remove_session(&result.stream_id.unwrap())
        .await;
}

fn signal(format: PcmFormat, windows: usize) -> Vec<u8> {
    let frames = format.sample_rate as usize * WINDOW_MS as usize / 1000;
    let mut pcm = Vec::new();
    for w in 0..windows {
        let amplitude = if w % 2 == 0 { 0.1 } else { 0.6 };
        for n in 0..frames {
            let v = amplitude
                * (n as f64 * 1000.0 * std::f64::consts::TAU / f64::from(format.sample_rate)).sin();
            for _ in 0..format.channels {
                match format.bit_depth {
                    16 => pcm.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
                    24 => pcm.extend_from_slice(&((v * 8388607.0) as i32).to_le_bytes()[..3]),
                    _ => unreachable!(),
                }
            }
        }
    }
    pcm
}

async fn next_level(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = rx.recv().await.unwrap();
            if event.event_type == "playback.audio_levels" {
                return event.data;
            }
        }
    })
    .await
    .expect("decoded PCM must feed real playback.audio_levels, not a silent spectrum")
}

#[tokio::test]
async fn decoded_pcm_preserves_wire_bytes_and_emits_real_spectrum_and_tap_at_seek() {
    for format in [
        cd(),
        PcmFormat {
            sample_rate: 96000,
            bit_depth: 24,
            channels: 1,
        },
    ] {
        let mut orch = test_orchestrator();
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        orch.event_bus = Some(bus);
        orch.playback.play(1, NowPlaying::default()).await;
        let mut tap = orch.playback.zone_tap(1).subscribe();
        let (tx, source) = fixture(format);
        let expected = signal(format, 4);
        let data = expected.clone();
        tokio::spawn(async move {
            // Intentionally split even individual samples, including s24 mono.
            for chunk in data.chunks(997) {
                tx.send(chunk.to_vec()).await.unwrap();
            }
        });
        let resolved = orch
            .serve_decoded_pcm(
                "independent-pcm",
                source,
                0,
                &PlayRequest {
                    zone_id: 1,
                    seek_ms: Some(30000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let sid = resolved.stream_id.unwrap();
        let session = orch
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .unwrap()
            .clone();
        assert_eq!(resolved.source, "independent-pcm");
        assert_eq!(
            session.info.wav_content_length(),
            None,
            "catalogue duration must not invent Content-Length"
        );
        assert_eq!(session.restart_position_ms.get(), Some(&30000));
        assert_eq!(
            session.decoded_source_format.get(),
            Some(&AudioFormat::Flac)
        );
        let received = tokio::time::timeout(Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            while let Some(chunk) = session.recv_chunk().await {
                bytes.extend(chunk);
            }
            bytes
        })
        .await
        .expect("finite PCM must expose real EOF");
        assert_eq!(
            received, expected,
            "analysis must not alter audio or lose split s24 frames"
        );
        let a = next_level(&mut events).await;
        let b = next_level(&mut events).await;
        assert_eq!(a["zone_id"], 1);
        assert_eq!(a["position_ms"], 30000);
        assert_eq!(b["position_ms"], 30040);
        assert_eq!(a["sample_rate"], format.sample_rate);
        assert_eq!(a["spectrum_frames"], format.sample_rate * 40 / 1000);
        assert!(
            b["rms_left"].as_f64().unwrap() > a["rms_left"].as_f64().unwrap() * 5.0,
            "spectrum must reflect changing PCM, not fabricated animation"
        );
        assert_ne!(a["spectrum_db"], b["spectrum_db"]);
        assert!(!a["spectrum_db"].as_array().unwrap().is_empty());
        let tapped = tap.recv().await.unwrap();
        assert_eq!(tapped.track_position.as_millis(), 30000);
        assert_eq!(tapped.format.bit_depth, format.bit_depth);
        assert_eq!(
            &*tapped.pcm,
            &expected[..format.sample_rate as usize * 40 / 1000 * format.frame_bytes()]
        );
        orch.streamer.remove_session(&sid).await;
    }
}

#[tokio::test]
async fn decoded_pcm_startup_failure_empty_and_partial_frame_leave_no_session() {
    for payload in [
        Ok(vec![]),
        Ok(vec![1, 2, 3]),
        Err("fixture decoder failed".to_string()),
    ] {
        let orch = test_orchestrator();
        let (tx, source) = fixture(cd());
        tokio::spawn(async move {
            match payload {
                Ok(bytes) => tx.send(bytes).await.unwrap(),
                Err(e) => tx.fail(e).await,
            }
        });
        let error = orch
            .serve_decoded_pcm(
                "fixture",
                source,
                0,
                &PlayRequest {
                    zone_id: 1,
                    ..Default::default()
                },
            )
            .await
            .err()
            .unwrap();
        assert!(
            error.contains("no PCM")
                || error.contains("inside an audio frame")
                || error.contains("fixture decoder failed"),
            "{error}"
        );
        assert!(
            orch.streamer.sessions_state().lock().await.is_empty(),
            "failed PCM startup must not leak a session"
        );
    }
}

#[tokio::test]
async fn decoded_pcm_stop_closes_backpressured_producer() {
    let orch = test_orchestrator();
    let (tx, source) = fixture(cd());
    let producer = tokio::spawn(async move {
        loop {
            if tx.send(vec![0; 65536]).await.is_err() {
                break;
            }
        }
    });
    let result = orch
        .serve_decoded_pcm(
            "fixture",
            source,
            0,
            &PlayRequest {
                zone_id: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // No HTTP consumer: the adapter is blocked in output.send().
    tokio::time::sleep(Duration::from_millis(100)).await;
    orch.streamer
        .remove_session(&result.stream_id.unwrap())
        .await;
    tokio::time::timeout(Duration::from_secs(1), producer)
        .await
        .expect("removing PCM session must cancel even a full producer pipe")
        .unwrap();
}

#[tokio::test]
async fn decoded_pcm_abandoned_resolution_closes_producer_and_session() {
    let orch = Arc::new(test_orchestrator());
    let (tx, source) = fixture(cd());
    let task_orch = orch.clone();
    let task = tokio::spawn(async move {
        task_orch
            .serve_decoded_pcm(
                "fixture",
                source,
                0,
                &PlayRequest {
                    zone_id: 1,
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while orch.streamer.sessions_state().lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(1), tx.closed())
        .await
        .expect("aborted resolution must close provider, not orphan worker");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !orch.streamer.sessions_state().lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn decoded_pcm_paused_analysis_is_bounded_and_replacement_cancels() {
    let mut orch = test_orchestrator();
    let bus = Arc::new(EventBus::new());
    let mut events = bus.subscribe();
    orch.event_bus = Some(bus);
    orch.playback.play(1, NowPlaying::default()).await;
    orch.playback.pause(1).await;
    let (tx, source) = fixture(cd());
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = sent.clone();
    let producer = tokio::spawn(async move {
        loop {
            if tx.send(vec![0; 65536]).await.is_err() {
                break;
            }
            count.fetch_add(65536, Ordering::Relaxed);
        }
    });
    let result = orch
        .serve_decoded_pcm(
            "fixture",
            source,
            0,
            &PlayRequest {
                zone_id: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let sid = result.stream_id.unwrap();
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .unwrap()
        .clone();
    let drain = tokio::spawn(async move { while session.recv_chunk().await.is_some() {} });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let first = sent.load(Ordering::Relaxed);
    assert!(
        first > 30 * 44100 * 4,
        "exercise the actual 30-second analysis bound"
    );
    assert!(
        first < 35 * 44100 * 4,
        "paused spectrum queue must not retain whole tracks: {first}"
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        sent.load(Ordering::Relaxed),
        first,
        "pause must backpressure the producer at a fixed memory ceiling"
    );
    assert!(
        events.try_recv().is_err(),
        "no analysis clock progression while paused"
    );
    orch.playback.bump_generation(1).await;
    tokio::time::timeout(Duration::from_secs(1), producer)
        .await
        .expect("new playback must cancel old PCM")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), drain)
        .await
        .unwrap()
        .unwrap();
    assert!(!orch.streamer.session_alive(&sid).await);
}

#[tokio::test]
async fn decoded_pcm_channel_validates_format_chunk_bound_and_drop_signal() {
    let (tx, source) = fixture(cd());
    assert!(
        tx.send(vec![0; 65537])
            .await
            .unwrap_err()
            .contains("exceeds limit")
    );
    drop(source);
    tokio::time::timeout(Duration::from_millis(100), tx.closed())
        .await
        .unwrap();
    for format in [
        PcmFormat {
            sample_rate: 0,
            ..cd()
        },
        PcmFormat {
            bit_depth: 32,
            ..cd()
        },
        PcmFormat {
            channels: 0,
            ..cd()
        },
    ] {
        assert!(format.validate().is_err());
    }
}
