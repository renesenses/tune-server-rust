//! Support premium : proxy authentifié vers l'API tickets de mozaiklabs.
//!
//! Le token OAuth premium (`mozaik_access_token`) vit côté serveur (settings) ;
//! le client web ne l'a jamais. Ces fonctions sont appelées par les handlers
//! `routes/support.rs`, sur le modèle de [`super::library_sync::push_changes`].
//! Le vrai gate premium est côté mozaiklabs (`auth.premium`) : un compte non
//! premium reçoit 403, propagé tel quel au client via [`SupportResult`].

use std::time::Duration;

use serde_json::{Value, json};

const SUPPORT_API: &str = "https://mozaiklabs.fr/api/v1/support/tickets";
const TIMEOUT: Duration = Duration::from_secs(30);

/// `Ok(body)` sur 2xx ; `Err((status, body))` sinon — le status HTTP de
/// mozaiklabs (401/403/422…) est préservé pour être renvoyé au client.
pub type SupportResult = Result<Value, (u16, Value)>;

/// Ouvre un ticket. Injecte automatiquement la version de Tune et l'OS —
/// le SAV voit la config sans la demander.
pub async fn create_ticket(
    http_client: &reqwest::Client,
    access_token: &str,
    subject: &str,
    body: &str,
    category: Option<&str>,
) -> SupportResult {
    let payload = json!({
        "subject": subject,
        "body": body,
        "category": category,
        "tune_version": crate::version(),
        "platform": std::env::consts::OS,
    });

    let resp = http_client
        .post(SUPPORT_API)
        .bearer_auth(access_token)
        .json(&payload)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Liste les tickets du compte premium.
pub async fn list_tickets(http_client: &reqwest::Client, access_token: &str) -> SupportResult {
    let resp = http_client
        .get(SUPPORT_API)
        .bearer_auth(access_token)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Détail d'un ticket (fil de messages inclus).
pub async fn get_ticket(
    http_client: &reqwest::Client,
    access_token: &str,
    id: i64,
) -> SupportResult {
    let resp = http_client
        .get(format!("{SUPPORT_API}/{id}"))
        .bearer_auth(access_token)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Ajoute une réponse client à un ticket (le rouvre côté SAV).
pub async fn reply(
    http_client: &reqwest::Client,
    access_token: &str,
    id: i64,
    body: &str,
) -> SupportResult {
    let resp = http_client
        .post(format!("{SUPPORT_API}/{id}/reply"))
        .bearer_auth(access_token)
        .json(&json!({ "body": body }))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// 502 Bad Gateway quand mozaiklabs est injoignable (réseau, timeout).
fn request_error(e: reqwest::Error) -> (u16, Value) {
    (
        502,
        json!({ "error": "support_upstream_unreachable", "detail": e.to_string() }),
    )
}

/// Préserve le status HTTP de mozaiklabs et son corps JSON (ou le texte brut
/// si la réponse n'est pas du JSON).
async fn parse(resp: reqwest::Response) -> SupportResult {
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| {
        (
            502u16,
            json!({ "error": "support_read_body", "detail": e.to_string() }),
        )
    })?;
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));

    if (200..300).contains(&status) {
        Ok(value)
    } else {
        Err((status, value))
    }
}
