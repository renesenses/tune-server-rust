use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;
use tracing::warn;

use crate::state::{CorpsRelaye, PendingResponse, RelayState};

pub async fn proxy_stream(
    State(state): State<Arc<RelayState>>,
    Path((server_id, stream_path)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let conn = match state.servers.get(&server_id) {
        Some(c) => c,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Auth : meme regle que le proxy d'API — `X-Bridge-Token` d'abord,
    // l'ancienne forme `Authorization: BridgeToken …` ensuite. Le commentaire
    // d'origine annoncait « from query or header » alors que seul l'en-tete
    // etait lu ; le lecteur audio du navigateur, lui, ne peut PAS poser
    // d'en-tete sur la source d'une balise <audio>, d'ou le parametre de
    // requete accepte ici.
    let jeton = crate::api_proxy::extraire_jeton(&headers).or_else(|| {
        query
            .get("token")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    });
    let auth_ok = jeton
        .as_deref()
        .map(|t| state.server_for_token(t).as_deref() == Some(&*server_id))
        .unwrap_or(false);

    if !auth_ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let stream_req = serde_json::json!({
        "type": "relay.stream_request",
        "id": request_id,
        "stream_id": stream_path,
        "range": range,
    });

    let (tx, rx) = oneshot::channel::<PendingResponse>();
    conn.pending.lock().await.insert(request_id.clone(), tx);

    if conn.ws_tx.send(stream_req.to_string()).await.is_err() {
        conn.pending.lock().await.remove(&request_id);
        return StatusCode::BAD_GATEWAY.into_response();
    }

    drop(conn);

    // Wait for the relay.stream_start response with headers/status
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(resp)) => reponse_relayee(resp),
        Ok(Err(_)) => {
            warn!(request_id = %request_id, "stream response channel dropped");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(_) => {
            if let Some(conn) = state.servers.get(&server_id) {
                conn.pending.lock().await.remove(&request_id);
            }
            warn!(request_id = %request_id, "stream request timeout");
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

/// Batit la reponse HTTP a partir de ce que le serveur a renvoye.
///
/// Un flux audio ne peut pas etre mis en memoire avant d'etre rendu : le
/// corps est cable directement sur le canal de morceaux, pour que le lecteur
/// entende le debut du morceau pendant que la fin voyage encore.
pub fn reponse_relayee(resp: PendingResponse) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);

    for (key, value) in &resp.headers {
        if let Some(v) = value.as_str() {
            response = response.header(key.as_str(), v);
        }
    }

    let corps = match resp.body {
        CorpsRelaye::Entier(body) => Body::from(body.unwrap_or_default()),
        CorpsRelaye::Morceaux(reception) => {
            let morceaux = futures_util::stream::unfold(reception, |mut rx| async move {
                rx.recv()
                    .await
                    .map(|morceau| (Ok::<Vec<u8>, std::io::Error>(morceau), rx))
            });
            Body::from_stream(morceaux)
        }
    };

    response
        .body(corps)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
