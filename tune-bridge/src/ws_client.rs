use std::sync::Arc;

use axum::extract::WebSocketUpgrade;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tracing::info;

use crate::sessions::{Acces, Rejet};
use crate::state::RelayState;
use crate::trafic::Categorie;
use tokio::sync::watch;

#[derive(Deserialize)]
pub struct ClientQuery {
    token: Option<String>,
}

pub async fn ws_client_handler(
    ws: WebSocketUpgrade,
    Path(server_id): Path<String>,
    Query(query): Query<ClientQuery>,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    // Jeton de session (#6603) ou ancien jeton du pont : voir
    // `RelayState::autoriser`.
    let acces = match state.autoriser(query.token.as_deref(), &server_id) {
        Ok(a) => a,
        Err(rejet) => return rejet.en_reponse(),
    };

    if !state.servers.contains_key(&server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let herite = matches!(acces, Acces::Herite);
    let mut reponse = ws
        .on_upgrade(move |socket| handle_client_ws(socket, server_id, state, acces))
        .into_response();
    if herite {
        reponse.headers_mut().insert(
            crate::sessions::ENTETE_AVERTISSEMENT,
            axum::http::HeaderValue::from_static("lien-obsolete"),
        );
    }
    reponse
}

/// Attend la coupure de la session ; jamais, pour le chemin herite.
async fn coupure_de(coupure: &mut Option<watch::Receiver<Option<Rejet>>>) -> Rejet {
    if let Some(rx) = coupure
        && let Ok(rejet) = rx.wait_for(|c| c.is_some()).await
    {
        return rejet.clone().unwrap_or_else(Rejet::non_autorise);
    }
    std::future::pending().await
}

/// Code de fermeture WebSocket d'une session coupee (plage applicative).
pub const FERMETURE_SESSION_COUPEE: u16 = 4001;

async fn handle_client_ws(
    socket: WebSocket,
    server_id: String,
    state: Arc<RelayState>,
    acces: Acces,
) {
    let client_id = uuid::Uuid::new_v4().to_string();
    info!(client_id = %client_id, server_id = %server_id, "remote client connected");
    let session = match &acces {
        Acces::Session(s) => Some(s.clone()),
        Acces::Herite => None,
    };
    let mut coupure = session.as_ref().map(|s| s.coupure());

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Notify server of new client
    if let Some(conn) = state.servers.get(&server_id) {
        let msg = serde_json::json!({
            "type": "relay.client_connected",
            "client_id": client_id,
        });
        let _ = conn.ws_tx.send(msg.to_string()).await;
    }

    // For now: forward subscription requests from client to server,
    // and relay events from server to client.
    // The server will tag events with client_id so the relay can route them.
    loop {
        let message = tokio::select! {
            m = ws_rx.next() => m,
            rejet = coupure_de(&mut coupure) => {
                // Le motif voyage dans la trame de fermeture : l'app sait
                // dire « L'ecoute a repris sur … ».
                let raison = serde_json::to_string(&rejet).unwrap_or_default();
                let raison: String = raison.chars().take(120).collect();
                let _ = ws_tx
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: FERMETURE_SESSION_COUPEE,
                        reason: raison.into(),
                    })))
                    .await;
                break;
            }
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                if let Some(s) = &session {
                    s.compter(text.len());
                }
                // Forward client messages to server (subscribe patterns, etc.)
                if let Some(conn) = state.servers.get(&server_id) {
                    let wrapped = serde_json::json!({
                        "type": "relay.client_message",
                        "client_id": client_id,
                        "message": text.to_string(),
                    })
                    .to_string();
                    // Entre : la trame du navigateur ; sort : le message
                    // vers le serveur (#6512).
                    let compteurs = conn.trafic.clone();
                    state.trafic.entree(&compteurs, Categorie::Api, text.len());
                    let n = wrapped.len();
                    if conn.ws_tx.send(wrapped).await.is_ok() {
                        state.trafic.sortie(&compteurs, Categorie::Api, n);
                    }
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws_tx.send(Message::Pong(data)).await;
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
            _ => {}
        }
    }

    // Notify server of client disconnect
    if let Some(conn) = state.servers.get(&server_id) {
        let msg = serde_json::json!({
            "type": "relay.client_disconnected",
            "client_id": client_id,
        });
        let _ = conn.ws_tx.send(msg.to_string()).await;
    }

    info!(client_id = %client_id, server_id = %server_id, "remote client disconnected");
}
