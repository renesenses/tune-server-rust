use super::*;

/// Server-owned onboarding for clients that already open verification_url.
/// GET only displays status; pairing requires an explicit POST.
pub(super) async fn page(State(state): State<StreamingHttpState>) -> Response {
    let service = match get_svc(&state, "spotify").await {
        Ok(service) => service,
        Err(error) => return error.into_response(),
    };
    if service
        .read()
        .await
        .auth_details()
        .as_ref()
        .and_then(|d| d["mode"].as_str())
        != Some("unofficial-native")
    {
        return (
            StatusCode::NOT_FOUND,
            "Native Spotify prototype is not enabled",
        )
            .into_response();
    }
    (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
            ("x-content-type-options", "nosniff"),
            ("content-security-policy", "default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'"),
        ],
        axum::response::Html(include_str!("spotify_pairing.html")),
    ).into_response()
}

pub(super) async fn script() -> Response {
    (
        [
            ("content-type", "text/javascript; charset=utf-8"),
            ("cache-control", "no-store"),
        ],
        include_str!("spotify_pairing.js"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> StreamingHttpState {
        StreamingHttpState::new(
            Arc::new(tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap()),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(EventBus::new()),
        )
    }
    #[tokio::test]
    async fn native_pairing_page_does_not_exist_without_the_native_service() {
        assert_eq!(page(State(state())).await.status(), StatusCode::NOT_FOUND);
    }
    #[tokio::test]
    async fn native_pairing_malformed_json_does_not_start_authentication() {
        let response = service_auth(
            State(state()),
            Path("spotify".into()),
            axum::body::Bytes::from_static(b"{bad"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"invalid authentication JSON");
    }
    #[tokio::test]
    async fn native_pairing_script_is_served_as_javascript() {
        let response = script().await;
        assert_eq!(
            response.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
}
