use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::{NowPlaying, PlayState};

async fn paused_browser() -> (AppState, i64, String) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Browser", Some("browser"), None)
        .unwrap();
    let queue = PlayQueueRepo::with_backend(state.backend.clone());
    let items: Vec<_> = ["Desert Mule", "Next track"]
        .into_iter()
        .enumerate()
        .map(|(i, title)| QueueInput::Streaming {
            source: "qobuz".into(),
            source_id: i.to_string(),
            title: title.into(),
            artist: "Artist".into(),
            album: None,
            cover_url: None,
            duration_ms: 300_000,
            track_number: None,
            disc_number: None,
        })
        .collect();
    queue.append(zid, &items).unwrap();
    let (sid, _tx, _ready) = state
        .streamer
        .create_session(Default::default(), false, 4)
        .await;
    state
        .playback
        .play(
            zid,
            NowPlaying {
                title: "Desert Mule".into(),
                source: "local".into(),
                duration_ms: 300_000,
                stream_id: Some(sid.clone()),
                ..Default::default()
            },
        )
        .await;
    state.playback.update_queue_info(zid, 0, 2).await;
    state.playback.pause(zid).await;
    (state, zid, sid)
}

async fn post_resume(state: &AppState, zid: i64, language: Option<&str>) -> (StatusCode, Value) {
    // Real route, header parsing, orchestrator, session registry and event bus.
    let app = router().with_state(state.clone());
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/{zid}/resume"));
    if let Some(language) = language {
        request = request.header("accept-language", language);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn i4193_expired_browser_session_uses_request_language_on_http_and_event() {
    let (state, zid, sid) = paused_browser().await;
    state.streamer.remove_session(&sid).await;
    let queue = PlayQueueRepo::with_backend(state.backend.clone());
    let before = serde_json::to_value(queue.get_ordered(zid).unwrap()).unwrap();
    let mut events = state.event_bus.subscribe();
    let (status, body) = post_resume(&state, zid, Some("en-GB,en;q=0.9,fr;q=0.8")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "output_command_failed");
    assert_eq!(body["command"], "resume");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("cannot resume where it stopped"),
        "the English browser must receive an English lost-session refusal: {message}"
    );
    assert!(message.contains("Desert Mule") && message.contains("Start the track again."));
    assert!(
        !message.contains("0:00"),
        "unknown browser position must not become zero: {message}"
    );
    let event = events
        .try_recv()
        .expect("the refusal must also reach other clients");
    assert_eq!(event.event_type, "zone.playback_error");
    assert_eq!(
        event.data["error"], body["message"],
        "HTTP and the event must carry the same translated refusal"
    );
    assert_eq!(event.data["code"], "stream_session_lost");
    assert_eq!(event.data["title"], "Desert Mule");
    assert!(event.data["position_ms"].is_null());
    assert_eq!(event.data["fatal"], true);
    let after = state.playback.get_state(zid).await;
    assert_eq!(after.state, PlayState::Paused);
    assert_eq!(after.queue_position, 0);
    assert_eq!(after.queue_length, 2);
    assert!(
        after.last_seek_at.is_none(),
        "a refusal must not seek to an invented position"
    );
    assert_eq!(
        serde_json::to_value(queue.get_ordered(zid).unwrap()).unwrap(),
        before,
        "a failed resume must preserve the queue"
    );
}

#[tokio::test]
async fn i4193_missing_language_keeps_a_french_refusal() {
    let (state, zid, sid) = paused_browser().await;
    state.streamer.remove_session(&sid).await;
    let (status, body) = post_resume(&state, zid, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("Relancez la piste.") && message.contains("navigateur"),
        "{message}"
    );
    assert!(!message.contains("0:00"));
}

#[tokio::test]
async fn i4193_a_live_browser_session_still_resumes_without_error() {
    let (state, zid, sid) = paused_browser().await;
    let mut events = state.event_bus.subscribe();
    let (status, _) = post_resume(&state, zid, Some("en")).await;
    assert_eq!(status, StatusCode::OK, "a live session must still resume");
    assert!(state.streamer.session_alive(&sid).await);
    assert_eq!(
        state.playback.get_state(zid).await.state,
        PlayState::Playing
    );
    while let Ok(event) = events.try_recv() {
        assert_ne!(event.event_type, "zone.playback_error");
    }
}

#[tokio::test]
async fn i4193_failed_restore_names_measured_position_and_cause_in_english() {
    let (state, zid, sid) = paused_browser().await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    // A measured position belongs to an output-backed zone. The missing
    // library track makes restoration fail before any hardware is contacted.
    let zid2 = repo
        .create("Output", Some("mock"), Some("mock-missing"))
        .unwrap();
    state
        .playback
        .play(
            zid2,
            NowPlaying {
                track_id: Some(987654321),
                title: "Lost track".into(),
                source: "local".into(),
                duration_ms: 300_000,
                stream_id: Some(sid.clone()),
                ..Default::default()
            },
        )
        .await;
    state.playback.update_position(zid2, 137_000).await;
    state.playback.pause(zid2).await;
    state.streamer.remove_session(&sid).await;
    let _ = zid;
    let (status, body) = post_resume(&state, zid2, Some("en")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("cannot resume at 2:17") && message.contains("Cause:"),
        "failed restoration must name its measured position and cause in English: {message}"
    );
}

#[test]
fn i4193_all_supported_locales_have_complete_session_messages() {
    let translations: Value = serde_json::from_str(include_str!("../../i18n_server.json")).unwrap();
    for lang in crate::i18n::SUPPORTED {
        for key in [
            "playback.sessionLostBrowser",
            "playback.sessionLostAtPosition",
            "playback.sessionLostCause",
        ] {
            assert!(
                translations[key][lang].as_str().is_some(),
                "missing {lang} translation of {key}"
            );
        }
        let message = session_message::lost_session(lang, "Desert Mule", None, None);
        assert!(
            message.contains("Desert Mule") && message.contains("30"),
            "{lang}: {message}"
        );
        assert!(
            !message.contains("0:00")
                && !message.contains("{title}")
                && !message.contains("{minutes}")
        );
    }
}

#[test]
fn i4193_titles_and_causes_are_not_reinterpreted_as_templates() {
    let message = session_message::lost_session(
        "en",
        "{position} / {minutes}",
        Some(137_000),
        Some("cause {title}"),
    );
    assert!(
        message.starts_with("“{position} / {minutes}” cannot resume at 2:17"),
        "{message}"
    );
    assert!(message.ends_with("Cause: cause {title}"), "{message}");
}

#[test]
fn i4193_known_zero_and_unknown_position_remain_distinct() {
    let known = session_message::lost_session("en", "Track", Some(0), None);
    let unknown = session_message::lost_session("en", "Track", None, None);
    assert!(known.contains("at 0:00"), "{known}");
    assert!(
        !unknown.contains("0:00") && unknown.contains("in the browser"),
        "{unknown}"
    );
}
