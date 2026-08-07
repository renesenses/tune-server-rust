//! Support premium : proxy authentifié vers l'API tickets de mozaiklabs.
//!
//! Deux façons de s'authentifier (voir [`SupportAuth`]) : le token OAuth premium
//! (`mozaik_access_token`, login SSO dans Tune) OU la clé de licence (premium par
//! clé, sans SSO — mozaiklabs résout le compte par l'e-mail de la licence). Le
//! client web n'a ni l'un ni l'autre → tout passe par le serveur, sur le modèle
//! de [`super::library_sync::push_changes`]. Le vrai gate premium est côté
//! mozaiklabs (`auth.premium`) : un compte non premium reçoit 403, propagé tel
//! quel au client via [`SupportResult`].

use std::time::Duration;

use serde_json::{Value, json};

const SUPPORT_API: &str = "https://mozaiklabs.fr/api/v1/support/tickets";
const TIMEOUT: Duration = Duration::from_secs(30);

/// `Ok(body)` sur 2xx ; `Err((status, body))` sinon — le status HTTP de
/// mozaiklabs (401/403/422…) est préservé pour être renvoyé au client.
pub type SupportResult = Result<Value, (u16, Value)>;

/// Authentification vers l'API support de mozaiklabs.
///
/// La plupart des testeurs premium le sont par CLÉ de licence et n'ont jamais
/// fait de login SSO : sans le chemin `License`, le support renvoyait 412 et
/// « ne fonctionnait pas » pour eux. mozaiklabs accepte les deux (voir
/// `PremiumApiAuth`) ; pour la clé il résout/crée le compte par l'e-mail de la
/// licence.
pub enum SupportAuth {
    /// Token OAuth premium (login SSO dans Tune).
    Bearer(String),
    /// Clé de licence + empreinte machine (informative).
    License { key: String, fingerprint: String },
}

impl SupportAuth {
    /// Applique l'auth à une requête sortante vers mozaiklabs.
    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            SupportAuth::Bearer(token) => req.bearer_auth(token),
            SupportAuth::License { key, fingerprint } => req
                .header("X-License-Key", key)
                .header("X-Hardware-Fingerprint", fingerprint),
        }
    }
}

/// Ouvre un ticket. Injecte automatiquement la version de Tune et l'OS —
/// le SAV voit la config sans la demander.
pub async fn create_ticket(
    http_client: &reqwest::Client,
    auth: &SupportAuth,
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

    let resp = auth
        .apply(http_client.post(SUPPORT_API))
        .json(&payload)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Une pièce jointe reçue du client web, prête à être relayée à mozaiklabs
/// telle quelle (le serveur Tune ne la stocke pas — il ne fait que la
/// transmettre dans le multipart sortant).
pub struct AttachmentUpload {
    /// Nom de fichier d'origine (ex. `capture.png`).
    pub file_name: String,
    /// Type MIME résolu (déjà validé côté serveur).
    pub content_type: String,
    /// Contenu brut du fichier.
    pub bytes: Vec<u8>,
}

/// Ouvre un ticket AVEC pièces jointes : relaie un `multipart/form-data` vers
/// mozaiklabs (mêmes champs que [`create_ticket`], plus `attachments[]`). La
/// version de Tune et l'OS sont injectés ici, jamais fournis par le client.
/// Les `fields` (subject/body/category/…) sont transmis tels quels ; l'appelant
/// (route serveur) a déjà validé nombre, taille et type des fichiers.
pub async fn create_ticket_multipart(
    http_client: &reqwest::Client,
    auth: &SupportAuth,
    fields: Vec<(String, String)>,
    files: Vec<AttachmentUpload>,
) -> SupportResult {
    let mut form = reqwest::multipart::Form::new()
        .text("tune_version", crate::version())
        .text("platform", std::env::consts::OS);

    for (name, value) in fields {
        form = form.text(name, value);
    }

    for file in files {
        let part = reqwest::multipart::Part::bytes(file.bytes)
            .file_name(file.file_name)
            .mime_str(&file.content_type)
            .map_err(|e| {
                (
                    400u16,
                    json!({ "error": "attachment_invalid_mime", "detail": e.to_string() }),
                )
            })?;
        // mozaiklabs attend `attachments[]` (règle Laravel `attachments.*`).
        form = form.part("attachments[]", part);
    }

    let resp = auth
        .apply(http_client.post(SUPPORT_API))
        .multipart(form)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Liste les tickets du compte premium.
pub async fn list_tickets(http_client: &reqwest::Client, auth: &SupportAuth) -> SupportResult {
    let resp = auth
        .apply(http_client.get(SUPPORT_API))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Détail d'un ticket (fil de messages inclus).
pub async fn get_ticket(
    http_client: &reqwest::Client,
    auth: &SupportAuth,
    id: i64,
) -> SupportResult {
    let resp = auth
        .apply(http_client.get(format!("{SUPPORT_API}/{id}")))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;

    parse(resp).await
}

/// Ajoute une réponse client à un ticket (le rouvre côté SAV).
pub async fn reply(
    http_client: &reqwest::Client,
    auth: &SupportAuth,
    id: i64,
    body: &str,
) -> SupportResult {
    let resp = auth
        .apply(http_client.post(format!("{SUPPORT_API}/{id}/reply")))
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
