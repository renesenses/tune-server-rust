use super::*;

#[tokio::test]
async fn malformed_json_does_not_start_provider_authentication() {
    let state = StreamingHttpState::new(
        Arc::new(tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap()),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(EventBus::new()),
    );
    let response = service_auth(
        State(state),
        Path("fixture-provider".into()),
        axum::body::Bytes::from_static(b"{bad"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"invalid authentication JSON");
}
