use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::playback::NowPlaying;

#[tokio::test]
async fn native_browser_stream_contract_is_consistent_on_every_zone_surface() {
    let state = AppState::new(":memory:", 18888, Default::default()).unwrap();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Browser", Some("browser"), None)
        .unwrap();
    let (stream_id, _tx, _) = state
        .streamer
        .create_session(
            StreamInfo {
                format: "wav".into(),
                sample_rate: 44100,
                bit_depth: 16,
                ..Default::default()
            },
            false,
            1,
        )
        .await;
    state.streamer.sessions_state().lock().await[&stream_id]
        .restart_position_ms
        .set(219000)
        .unwrap();
    state
        .playback
        .play(
            zone_id,
            NowPlaying {
                source: "spotify".into(),
                stream_id: Some(stream_id.clone()),
                format: Some("flac".into()),
                sample_rate: Some(44100),
                bit_depth: Some(16),
                ..Default::default()
            },
        )
        .await;
    let app = crate::routes::router(state.clone());
    for path in [
        "/api/v1/zones".to_string(),
        format!("/api/v1/zones/{zone_id}"),
        format!("/api/v1/zones/{zone_id}/status"),
    ] {
        let reply = app
            .clone()
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK, "{path}");
        let mut body: Value =
            serde_json::from_slice(&to_bytes(reply.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        if body.is_array() {
            body = body
                .as_array()
                .unwrap()
                .iter()
                .find(|zone| zone["id"] == zone_id)
                .unwrap()
                .clone();
        }
        assert_eq!(
            body["browser_stream"]["start_position_ms"], 219000,
            "PCM byte zero needs its track offset on {path}"
        );
        assert_eq!(body["browser_stream"]["seek_mode"], "restart");
        // /status serializes ZoneState (now_playing), whereas the list and
        // detail expose current_track plus the enriched signal path.
        if path.ends_with("/status") {
            assert_eq!(body["now_playing"]["format"], "flac");
        } else {
            assert_eq!(
                body["current_track"]["format"], "flac",
                "source codec must survive every HTTP surface: {path}"
            );
            assert_eq!(body["signal_path"]["source_format"], "FLAC");
            assert_eq!(body["signal_path"]["transport_format"], "WAV");
            assert_eq!(body["signal_path"]["lossless"], true);
            assert_eq!(body["signal_path"]["bit_perfect"], false);
        }
        assert!(
            body["stream_url"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/{stream_id}.wav")),
            "GET and play must use the same WAV URL, otherwise resume steals the one-shot stream: {path}"
        );
    }
    let play_body = crate::routes::playback::build_zone_json(&state, zone_id).await;
    assert_eq!(play_body["browser_stream"]["start_position_ms"], 219000);
    assert_eq!(play_body["current_track"]["format"], "flac");
    assert_eq!(play_body["signal_path"]["source_format"], "FLAC");
    assert_eq!(play_body["signal_path"]["transport_format"], "WAV");
}

#[tokio::test]
async fn native_browser_contract_does_not_leak_to_renderers_or_regular_files() {
    let state = AppState::new(":memory:", 18888, Default::default()).unwrap();
    let (id, _tx, _) = state
        .streamer
        .create_session(StreamInfo::default(), false, 1)
        .await;
    let mut regular = serde_json::Map::new();
    assert!(inject_stream_url(&mut regular, &state, Some("browser"), Some(&id)).await);
    assert!(!regular.contains_key("browser_stream"));
    assert!(regular["stream_url"].as_str().unwrap().ends_with(".flac"));
    state.streamer.sessions_state().lock().await[&id]
        .restart_position_ms
        .set(0)
        .unwrap();
    for output in [Some("dlna"), Some("local"), None] {
        let mut renderer = serde_json::Map::new();
        assert!(!inject_stream_url(&mut renderer, &state, output, Some(&id)).await);
        assert!(
            renderer.is_empty(),
            "a browser must not consume another renderer's PCM pipe"
        );
    }
}
