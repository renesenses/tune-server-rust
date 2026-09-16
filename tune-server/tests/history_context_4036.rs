use axum::{body::Body, http::Request};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

async fn history(s: &AppState, query: &str) -> Value {
    let response = tune_server::routes::router(s.clone())
        .oneshot(
            Request::get(format!("/api/v1/library/history?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn fixture() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    s.backend
        .execute(
            "INSERT INTO playlists (id,name,profile_id) VALUES (42,'Soirée',1)",
            &[],
        )
        .unwrap();
    for (id, source, date) in [
        (1_i64, "local", "2026-09-15 12:00:02"),
        (2, "qobuz", "2026-09-15 12:00:01"),
    ] {
        s.backend.execute("INSERT INTO listen_history (id,title,artist_name,album_title,source,source_id,duration_ms,context_type,context_id,context_position,profile_id,cover_url,listened_at) VALUES (?,'Track','Artist','Album',?,'track-42',180000,'playlist','42',3,1,'https://example.test/cover.jpg',?)", &[&id,&source,&date]).unwrap();
    }
    s
}

#[tokio::test]
async fn i4036_api_adds_playlist_name_without_changing_history_or_cover() {
    let s = fixture();
    let v = history(&s, "limit=1&offset=0").await;
    assert_eq!(v["total"], 2);
    assert_eq!(v["limit"], 1);
    assert_eq!(v["offset"], 0);
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    let item = &v["items"][0];
    assert_eq!(item["id"], 1);
    assert_eq!(item["context_name"], "Soirée");
    assert_eq!(item["context_type"], "playlist");
    assert_eq!(item["context_id"], "42");
    assert_eq!(item["context_position"], 3);
    assert_eq!(item["title"], "Track");
    assert_eq!(item["artist_name"], "Artist");
    assert_eq!(item["album_title"], "Album");
    assert_eq!(item["duration_ms"], 180000);
    // The legacy history reader does not expose the stored profile.
    assert!(item["profile_id"].is_null());
    assert_eq!(item["cover_url"], "https://example.test/cover.jpg");

    let next = history(&s, "limit=1&offset=1").await;
    assert_eq!(next["total"], 2);
    assert_eq!(next["items"][0]["id"], 2);
    assert!(
        next["items"][0].get("context_name").unwrap().is_null(),
        "a numeric Qobuz ID must never inherit a local playlist name"
    );
    assert_eq!(
        next["items"][0]["cover_url"],
        "https://example.test/cover.jpg"
    );
}
#[tokio::test]
async fn i4036_api_uses_current_name_and_preserves_deleted_playlist_history() {
    let s = fixture();
    s.backend
        .execute("UPDATE playlists SET name='Nouveau nom' WHERE id=42", &[])
        .unwrap();
    assert_eq!(
        history(&s, "limit=1").await["items"][0]["context_name"],
        "Nouveau nom"
    );
    s.backend
        .execute("DELETE FROM playlists WHERE id=42", &[])
        .unwrap();
    let v = history(&s, "limit=1").await;
    assert_eq!(v["total"], 2);
    assert_eq!(v["items"][0]["context_id"], "42");
    assert!(v["items"][0].get("context_name").unwrap().is_null());
    assert!(
        history(&s, "limit=1&offset=99").await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
