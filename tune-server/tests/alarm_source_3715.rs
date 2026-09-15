use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

async fn request(s: &AppState, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let r = tune_server::routes::router(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}
async fn verify(s: AppState, source: &str, replacement: &str) {
    let (status,v)=request(&s,Method::POST,"/api/v1/alarms",json!({"name":"Source test","time":"07:30","source_type":"playlist","source_id":source,"volume":0.2})).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    assert!(id > 0);
    let scheduler =
        tune_core::alarms::AlarmScheduler::with_backend(s.backend.clone(), s.orchestrator.clone());
    assert_eq!(
        scheduler.get_alarm(id).unwrap().unwrap()["source_id"],
        source
    );
    let (status, v) = request(&s, Method::GET, "/api/v1/alarms", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        v.as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()["source_id"],
        source
    );
    let (status, v) = request(
        &s,
        Method::PUT,
        &format!("/api/v1/alarms/{id}"),
        json!({"source_id":replacement}),
    )
    .await;
    assert!(status.is_success(), "{status}: {v}");
    assert_eq!(
        scheduler.get_alarm(id).unwrap().unwrap()["source_id"],
        replacement
    );
    let (_, v) = request(&s, Method::GET, "/api/v1/alarms", Value::Null).await;
    assert_eq!(
        v.as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()["source_id"],
        replacement
    );
}
#[tokio::test]
async fn i3715_alarm_numeric_sqlite_ids_survive_list_and_scheduler() {
    verify(
        AppState::new(":memory:", 0, Default::default()).unwrap(),
        "42",
        "73",
    )
    .await;
}
#[tokio::test]
async fn i3715_alarm_opaque_sqlite_ids_survive_list_and_scheduler() {
    verify(
        AppState::new(":memory:", 0, Default::default()).unwrap(),
        "playlist:abc",
        "playlist:next",
    )
    .await;
}
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn i3715_alarm_postgres_create_update_list_and_scheduler() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    let root = AppState::new(
        "",
        0,
        tune_server::config::TuneConfig {
            database_url: Some(url.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let name = format!(
        "tune_alarm_api_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    root.backend
        .execute(&format!("CREATE DATABASE {name}"), &[])
        .unwrap();
    let prefix = url.rsplit_once('/').unwrap().0;
    let s = AppState::new(
        "",
        0,
        tune_server::config::TuneConfig {
            database_url: Some(format!("{prefix}/{name}")),
            ..Default::default()
        },
    )
    .unwrap();
    verify(s, "000123", "qobuz:playlist:abc").await;
    root.backend
        .execute(&format!("DROP DATABASE {name} WITH (FORCE)"), &[])
        .unwrap();
}
