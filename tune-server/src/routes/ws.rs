use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::time::interval;

use crate::state::AppState;

const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Evenements re-cadences PAR CLIENT avant expedition.
///
/// Deuxieme filet, derriere celui de l'emetteur : un client lent ne doit pas se
/// faire distancer par un flot d'avancement au point de perdre, par `Lagged`,
/// les evenements qui comptent (fin de scan, changement de zone). Ne sont
/// cadences que des evenements IDEMPOTENTS — en perdre un est sans consequence,
/// le suivant porte l'etat complet.
///
/// `device.updated` en fait partie depuis #2870 : le mDNS re-resout un appareil
/// a chaque rafraichissement de bail, et parfois a chaque changement d'etat de
/// l'enceinte. Le client ne fait qu'y recharger sa liste d'appareils.
const EVENEMENTS_CADENCES: &[&str] = &[
    "library.scan.progress",
    "library.enrich.progress",
    "library.artwork.progress",
    "device.updated",
];

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(ws_handler))
}

/// Authorizes the WS upgrade *before* the `WebSocketUpgrade` extractor runs, so
/// an unauthenticated caller is rejected with 401 without ever reaching the
/// upgrade. WS routers are mounted outside the API auth middleware, so without
/// this the socket would receive the full snapshot and live event stream with
/// no token. Running as a leading extractor also makes the gate unit-testable:
/// a synthetic `oneshot` request carries no hyper upgrade state, so
/// `WebSocketUpgrade` itself always fails there — only a check that runs first
/// can exercise the 401 path.
struct WsAuthorized;

impl axum::extract::FromRequestParts<AppState> for WsAuthorized {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let query_token = parts.uri.query().and_then(ws_token_from_query);
        if crate::auth::ws_authorized(state, &parts.headers, query_token.as_deref()) {
            Ok(WsAuthorized)
        } else {
            Err((
                axum::http::StatusCode::UNAUTHORIZED,
                "authentication required",
            )
                .into_response())
        }
    }
}

/// Extract `token` / `access_token` from a raw query string. JWTs are URL-safe
/// (base64url + `.`), so no percent-decoding is required.
fn ws_token_from_query(raw: &str) -> Option<String> {
    for pair in raw.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k == "token" || k == "access_token" {
            return Some(it.next().unwrap_or("").to_string());
        }
    }
    None
}

async fn ws_handler(
    _auth: WsAuthorized,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn matches_pattern(event_type: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.ends_with(".*") {
        let prefix = &pattern[..pattern.len() - 2];
        return event_type.starts_with(prefix);
    }
    event_type == pattern
}

/// Build the full current state sent to a client on connect (`type: "snapshot"`).
/// Merges persisted zone metadata (name/online/type/group) with live playback
/// state (transport, volume, now-playing, queue) so the client renders the
/// truth without polling.
async fn build_snapshot(state: &AppState) -> serde_json::Value {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zones = zone_repo.list().unwrap_or_default();
    #[cfg(feature = "local-audio")]
    let audio_backend =
        tune_core::outputs::local::active_backend_name(&state.display_audio_backend());
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    let devices = state.scanner.devices().await;
    let mut zone_snaps = Vec::with_capacity(zones.len());
    for z in &zones {
        let zid = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zid).await;
        let renderer_label = z
            .output_device_id
            .as_deref()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
        let wire = match ps
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
        {
            Some(sid) => state.streamer.stream_output_wire(sid).await,
            None => None,
        };
        let signal_path = crate::routes::zones::build_signal_path_pub(
            &ps,
            z,
            &state.backend,
            renderer_label,
            audio_backend,
            wire.as_ref(),
        );
        let output_capabilities =
            crate::routes::zones::output_capabilities(state, z.output_device_id.as_deref()).await;
        zone_snaps.push(serde_json::json!({
            "zone_id": zid,
            "name": z.name,
            "online": z.online,
            "output_type": z.output_type,
            "group_id": z.group_id,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "volume": ps.volume,
            "muted": ps.muted,
            "shuffle": ps.shuffle,
            "repeat": ps.repeat,
            "position_ms": ps.position_ms,
            "queue_position": ps.queue_position,
            "queue_length": ps.queue_length,
            "now_playing": ps.now_playing,
            "signal_path": signal_path,
            "output_capabilities": output_capabilities,
            "resolving": ps.resolving,
        }));
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: serde_json::Value = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!([]));

    serde_json::json!({
        "type": "snapshot",
        "data": { "zones": zone_snaps, "groups": groups },
    })
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.playback.subscribe();
    let mut event_rx = state.event_bus.subscribe();
    let mut patterns: Vec<String> = vec!["*".to_string()];
    let mut ping_interval = interval(PING_INTERVAL);
    ping_interval.tick().await;
    let mut cadences: std::collections::HashMap<&'static str, tune_core::cadence::Cadence> =
        EVENEMENTS_CADENCES
            .iter()
            .map(|nom| (*nom, tune_core::cadence::Cadence::avancement()))
            .collect();

    // Snapshot-on-connect: hand the client the full current state up front so
    // it has the truth immediately, instead of a blank UI until the next event
    // (or a separate REST round-trip). Subscriptions above are already live, so
    // any change during snapshot building is buffered and delivered as a delta.
    {
        let snapshot = build_snapshot(&state).await;
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        let event_type = format!("playback.{}", ev.event);

                        if !patterns.iter().any(|p| matches_pattern(&event_type, p)) {
                            continue;
                        }

                        let mut data = ev.data.clone();
                        if let Some(obj) = data.as_object_mut() {
                            obj.insert("zone_id".into(), serde_json::json!(ev.zone_id));
                        }
                        let ws_event = serde_json::json!({
                            "type": event_type,
                            "data": data,
                        });
                        let json = serde_json::to_string(&ws_event).unwrap_or_default();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket playback broadcast lagged, skipped {n} messages");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("WebSocket broadcast closed, resubscribing");
                        rx = state.playback.subscribe();
                        continue;
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(ev) => {
                        if !patterns.iter().any(|p| matches_pattern(&ev.event_type, p)) {
                            continue;
                        }
                        // Throttle idempotent progress/refresh events to max 1
                        // per 2s per client (see EVENEMENTS_CADENCES).
                        if let Some(cadence) = cadences.get_mut(ev.event_type.as_str())
                            && !cadence.autorise()
                        {
                            continue;
                        }
                        let ws_event = serde_json::json!({
                            "type": ev.event_type,
                            "data": ev.data,
                        });
                        let json = serde_json::to_string(&ws_event).unwrap_or_default();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket event_bus broadcast lagged, skipped {n} messages");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("WebSocket event_bus broadcast closed, resubscribing");
                        event_rx = state.event_bus.subscribe();
                        continue;
                    }
                }
            }
            _ = ping_interval.tick() => {
                // Keepalive. Send BOTH a protocol Ping (standards-compliant native
                // clients auto-Pong) AND an app-level text ping the web client
                // matches exactly (`{"type":"ping"}` → it replies "pong"). Protocol
                // Ping frames are invisible to browser JS and are stripped/ignored
                // by some proxies / VPNs / webviews, so on their own they don't
                // keep the app-level channel alive through an intermediary: the
                // socket is torn down on the intermediary's idle timeout every
                // ~15s and the client reconnects in a loop (Jean Marie, macOS). A
                // real Text frame forces data through every hop and defeats idle
                // timeouts. The client's "pong" reply is non-JSON text and is
                // harmlessly ignored by the recv arm below.
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
                if socket
                    .send(Message::Text("{\"type\":\"ping\"}".into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&text) {
                            // Support both formats:
                            // v1: {"subscribe": ["pattern1", "pattern2"]}
                            // v2: {"action": "subscribe", "patterns": ["pattern1", "pattern2"]}
                            let subs = cmd.get("subscribe").and_then(|v| v.as_array())
                                .or_else(|| {
                                    if cmd.get("action").and_then(|v| v.as_str()) == Some("subscribe") {
                                        cmd.get("patterns").and_then(|v| v.as_array())
                                    } else {
                                        None
                                    }
                                });
                            if let Some(subs) = subs {
                                patterns = subs.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect();
                                if patterns.is_empty() {
                                    patterns.push("*".to_string());
                                }
                                let ack = serde_json::json!({"type": "subscribed", "patterns": &patterns});
                                let _ = socket.send(Message::Text(
                                    serde_json::to_string(&ack).unwrap_or_default().into()
                                )).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests_cadence {
    use super::*;

    /// Le re-cadencement par client ne doit toucher QUE des evenements
    /// idempotents : en perdre un ne perd aucune information, le suivant porte
    /// l'etat complet. Un `library.scan.completed` cadence, lui, ferait
    /// disparaitre la banniere de fin de scan.
    #[test]
    fn seuls_des_evenements_idempotents_sont_cadences() {
        for nom in EVENEMENTS_CADENCES {
            assert!(
                nom.ends_with(".progress") || *nom == "device.updated",
                "{nom} n'est pas un evenement d'avancement : le cadencer \
                 ferait PERDRE de l'information au client"
            );
        }
        for interdit in [
            "library.scan.completed",
            "library.enrich.completed",
            "library.artwork.completed",
            "device.discovered",
            "device.lost",
            "zone.updated",
        ] {
            assert!(
                !EVENEMENTS_CADENCES.contains(&interdit),
                "{interdit} ne doit JAMAIS etre cadence"
            );
        }
    }

    /// Les quatre noms cadences existent bien dans `EventType` : une faute de
    /// frappe ici serait invisible (le filtre ne s'appliquerait a rien).
    #[test]
    fn chaque_nom_cadence_est_un_evenement_declare() {
        for nom in EVENEMENTS_CADENCES {
            assert!(
                tune_core::event_types::EventType::TOUTES
                    .iter()
                    .any(|e| e.as_str() == *nom),
                "« {nom} » ne correspond a aucune variante d'EventType"
            );
        }
    }

    /// Le filtre laisse passer la premiere annonce, retient la deuxieme dans la
    /// foulee, et laisse repasser apres l'intervalle. C'est exactement la regle
    /// que `library.scan.progress` appliquait deja a la main.
    #[test]
    fn le_filtre_espace_sans_tout_bloquer() {
        use std::time::Instant;
        let t0 = Instant::now();
        let mut c = tune_core::cadence::Cadence::avancement();
        assert!(c.autorise_a(t0));
        assert!(!c.autorise_a(t0 + Duration::from_millis(500)));
        assert!(c.autorise_a(t0 + Duration::from_secs(3)));
    }
}
