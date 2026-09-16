use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

async fn detail(state: &AppState) -> Value {
    let r = tune_server::routes::router(state.clone())
        .oneshot(
            Request::get("/api/v1/library/albums/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(r.status().is_success());
    serde_json::from_slice(
        &axum::body::to_bytes(r.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn state() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    s.backend
        .execute(
            "INSERT INTO albums (id,title,source) VALUES (1,'Provenance','local')",
            &[],
        )
        .unwrap();
    for id in 1_i64..=2 {
        s.backend
            .execute(
                "INSERT INTO tracks (id,title,album_id) VALUES (?,'Track',1)",
                &[&id],
            )
            .unwrap();
    }
    s
}
#[tokio::test]
async fn i3924_api_exposes_the_actual_mixed_average_then_album_tag() {
    let s = state();
    for (id, dr, source) in [(1_i64, "10", "tag"), (2, "14", "analysis")] {
        for (key, value) in [("dr_track", dr), ("dr_source", source)] {
            s.backend
                .execute(
                    "INSERT INTO track_metadata (track_id,key,value) VALUES (?,?,?)",
                    &[&id, &key, &value],
                )
                .unwrap();
        }
    }
    let v = detail(&s).await;
    assert_eq!(v["dynamic_range"], "12");
    assert_eq!(v["dynamic_range_source"], "track_average");
    assert_eq!(
        v["dynamic_range_provenance"],
        json!({"source":"mixed","tag_tracks":1,"analysis_tracks":1,"unknown_tracks":0})
    );
    s.backend
        .execute(
            "INSERT INTO track_metadata (track_id,key,value) VALUES (1,'dr_album','0')",
            &[],
        )
        .unwrap();
    let v = detail(&s).await;
    assert_eq!(v["dynamic_range"], "0");
    assert_eq!(v["dynamic_range_source"], "album_tag");
    assert_eq!(
        v["dynamic_range_provenance"],
        json!({"source":"tag","tag_tracks":0,"analysis_tracks":0,"unknown_tracks":0})
    );
}
#[tokio::test]
async fn i3924_api_distinguishes_no_measurement_from_unknown_origin() {
    let s = state();
    let v = detail(&s).await;
    assert!(v.get("dynamic_range").is_none());
    assert!(v.get("dynamic_range_provenance").is_none());
    s.backend
        .execute(
            "INSERT INTO track_metadata (track_id,key,value) VALUES (1,'dr_track','12')",
            &[],
        )
        .unwrap();
    let v = detail(&s).await;
    assert_eq!(
        v["dynamic_range_provenance"],
        json!({"source":"unknown","tag_tracks":0,"analysis_tracks":0,"unknown_tracks":1})
    );
}
