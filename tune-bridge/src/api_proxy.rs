use std::sync::Arc;
use std::time::Duration;

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

    // Auth : jeton de pont, sur son PROPRE en-tete.
    //
    // Il vivait dans `Authorization: BridgeToken …`, et cet en-tete etait
    // ensuite retransmis tel quel au serveur. Or le serveur, quand
    // `auth_enabled` vaut true, attend `Authorization: Bearer <jwt>` au meme
    // endroit : les deux ne peuvent pas coexister. Un utilisateur qui protege
    // son serveur — ce que tout acces depuis Internet devrait imposer — se
    // retrouvait avec un relais qui mange l'en-tete dont le serveur a besoin.
    //
    // `X-Bridge-Token` separe les deux. L'ancienne forme reste acceptee : la
    // premiere version de tune-remote l'utilise, et casser un client deja
    // livre pour une question de propriete serait mal echange.
    let token = extraire_jeton(&headers);
    match token.as_deref() {
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
                // Ne PAS transmettre un `Authorization` qui porte le jeton de
                // pont : il ne concerne que le relais, et le serveur y
                // chercherait un `Bearer`. Un vrai `Bearer` destine au serveur
                // passe, lui, sans y toucher.
                if key == "authorization" && porte_un_jeton_de_pont(v) {
                    continue;
                }
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
        Ok(Ok(resp)) => crate::stream_proxy::reponse_relayee(resp),
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

/// En-tete dedie au jeton de pont.
pub const BRIDGE_TOKEN_HEADER: &str = "x-bridge-token";

/// Vrai si cette valeur d'`Authorization` porte un jeton de pont — donc si
/// elle s'adresse au relais et non au serveur.
pub fn porte_un_jeton_de_pont(valeur: &str) -> bool {
    let v = valeur.trim_start();
    v.len() >= 12 && v[..12].eq_ignore_ascii_case("BridgeToken ")
}

/// Jeton presente par le client, quelle que soit la forme.
///
/// `X-Bridge-Token` d'abord ; a defaut l'ancienne forme
/// `Authorization: BridgeToken …`, conservee pour les clients deja livres.
pub fn extraire_jeton(headers: &HeaderMap) -> Option<String> {
    if let Some(t) = headers
        .get(BRIDGE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some(t.to_string());
    }
    let auth = headers.get("authorization")?.to_str().ok()?;
    if !porte_un_jeton_de_pont(auth) {
        return None;
    }
    let t = auth.trim_start()[12..].trim();
    (!t.is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod jeton_de_pont_tests {
    use super::*;
    use axum::http::HeaderValue;

    fn entetes(paires: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in paires {
            // `HeaderName::from_bytes` plutot que `insert(*k, …)` : ce dernier
            // exige un nom 'static, que des &str de test n'ont pas.
            let nom = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
            h.insert(nom, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn len_tete_dedie_est_lu() {
        let h = entetes(&[(BRIDGE_TOKEN_HEADER, "jeton-1")]);
        assert_eq!(extraire_jeton(&h).as_deref(), Some("jeton-1"));
    }

    /// La premiere version de tune-remote envoie l'ancienne forme. La casser
    /// pour une question de proprete serait mal echange.
    #[test]
    fn lancienne_forme_reste_acceptee() {
        let h = entetes(&[("authorization", "BridgeToken jeton-2")]);
        assert_eq!(extraire_jeton(&h).as_deref(), Some("jeton-2"));
    }

    #[test]
    fn len_tete_dedie_prime_sur_lancienne_forme() {
        let h = entetes(&[
            (BRIDGE_TOKEN_HEADER, "neuf"),
            ("authorization", "BridgeToken ancien"),
        ]);
        assert_eq!(extraire_jeton(&h).as_deref(), Some("neuf"));
    }

    /// LE point de ce changement : un `Bearer` s'adresse au SERVEUR, pas au
    /// relais. Le confondre avec un jeton de pont reviendrait a refuser
    /// l'acces a un utilisateur qui a protege son serveur.
    #[test]
    fn un_bearer_nest_pas_un_jeton_de_pont() {
        let h = entetes(&[("authorization", "Bearer eyJhbGciOi.jwt.signature")]);
        assert_eq!(extraire_jeton(&h), None);
        assert!(!porte_un_jeton_de_pont("Bearer eyJhbGciOi.jwt.signature"));
    }

    /// Le serveur attend `Authorization: Bearer <jwt>` quand `auth_enabled`
    /// vaut true. Si le relais lui transmettait son propre jeton au meme
    /// endroit, le serveur chercherait un Bearer et n'en trouverait pas :
    /// acces refuse, sans que rien n'explique pourquoi.
    #[test]
    fn seul_le_jeton_de_pont_est_reconnu_comme_tel() {
        assert!(porte_un_jeton_de_pont("BridgeToken abc"));
        assert!(porte_un_jeton_de_pont("bridgetoken abc"));
        assert!(porte_un_jeton_de_pont("  BridgeToken abc"));
        assert!(!porte_un_jeton_de_pont("Basic dXNlcjpwYXNz"));
        assert!(!porte_un_jeton_de_pont(""));
        assert!(!porte_un_jeton_de_pont("BridgeTokenSansEspace"));
    }

    #[test]
    fn un_jeton_vide_vaut_absence() {
        assert_eq!(
            extraire_jeton(&entetes(&[(BRIDGE_TOKEN_HEADER, "  ")])),
            None
        );
        assert_eq!(
            extraire_jeton(&entetes(&[("authorization", "BridgeToken   ")])),
            None
        );
    }

    #[test]
    fn sans_rien_aucun_jeton() {
        assert_eq!(extraire_jeton(&HeaderMap::new()), None);
    }
}
