use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;
use tracing::warn;

use crate::state::{PendingResponse, RelayState};

pub async fn proxy_api(
    State(state): State<Arc<RelayState>>,
    Path((server_id, path)): Path<(String, String)>,
    headers: HeaderMap,
    method: axum::http::Method,
    body: axum::body::Bytes,
) -> Response {
    // Validate server exists
    let conn = match state.servers.get(&server_id) {
        Some(c) => c,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Auth: check bridge_token from Authorization header
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth
        .strip_prefix("BridgeToken ")
        .or_else(|| auth.strip_prefix("bridgetoken "));
    match token {
        Some(t) if state.server_for_token(t).as_deref() == Some(&server_id) => {}
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    }

    let request_id = uuid::Uuid::new_v4().to_string();

    // Build relay headers (forward relevant ones)
    let mut relay_headers = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let key = name.as_str();
        if matches!(key, "content-type" | "accept" | "authorization" | "range") {
            if let Ok(v) = value.to_str() {
                relay_headers.insert(key.to_string(), serde_json::Value::String(v.to_string()));
            }
        }
    }

    let body_str = if body.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&body).into_owned())
    };

    let request_msg = serde_json::json!({
        "type": "relay.request",
        "id": request_id,
        "method": method.as_str(),
        "path": format!("/api/v1/{path}"),
        "headers": relay_headers,
        "body": body_str,
    });

    // Register pending response
    let (tx, rx) = oneshot::channel::<PendingResponse>();
    conn.pending.lock().await.insert(request_id.clone(), tx);

    // Send to server
    if conn.ws_tx.send(request_msg.to_string()).await.is_err() {
        conn.pending.lock().await.remove(&request_id);
        return StatusCode::BAD_GATEWAY.into_response();
    }

    drop(conn);

    // Wait for response with timeout
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(resp)) => {
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = Response::builder().status(status);

            for (key, value) in &resp.headers {
                if let Some(v) = value.as_str() {
                    response = response.header(key.as_str(), v);
                }
            }

            let body = resp.body.unwrap_or_default();
            response
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(Err(_)) => {
            warn!(request_id = %request_id, "response channel dropped");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(_) => {
            if let Some(conn) = state.servers.get(&server_id) {
                conn.pending.lock().await.remove(&request_id);
            }
            warn!(request_id = %request_id, "relay request timeout (30s)");
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}
