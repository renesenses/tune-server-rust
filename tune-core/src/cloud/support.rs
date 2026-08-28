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
    // Les en-têtes se lisent AVANT `resp.text()`, qui consomme la réponse.
    let retry_after = retry_after_secs(resp.headers());
    let text = resp.text().await.map_err(|e| {
        (
            502u16,
            json!({ "error": "support_read_body", "detail": e.to_string() }),
        )
    })?;

    build_result(status, &text, retry_after)
}

/// Délai avant nouvelle tentative annoncé par mozaiklabs, en secondes.
///
/// `Retry-After` (RFC 9110) est posé en delta-secondes par le limiteur de
/// Laravel ; `X-RateLimit-Reset` (horodatage Unix) sert de repli quand le
/// premier manque. La forme HTTP-date de `Retry-After` n'est pas interprétée :
/// mozaiklabs n'en émet pas, et deviner vaut moins que ne rien dire.
///
/// Rend `None` si rien n'est exploitable — l'interface affiche alors un
/// message sans délai, jamais un délai inventé (#2178).
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(secs) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        // 0 signifie « réessaie tout de suite » : ce n'est pas un délai à
        // afficher, et « réessaie dans 0 seconde » serait absurde.
        return (secs > 0).then_some(secs);
    }

    let reset = headers
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    let now = chrono::Utc::now().timestamp();
    (reset > now).then(|| (reset - now) as u64)
}

/// Construit le `SupportResult` à partir du statut, du corps brut et du délai
/// lu dans les en-têtes.
///
/// Sur **429** (limite d'envoi du relais mozaiklabs), le corps est enrichi de
/// deux champs que le client n'avait pas : `error: "rate_limited"`, motif
/// stable à traduire, et `retry_after`, le nombre de secondes avant nouvelle
/// tentative quand mozaiklabs l'annonce. Sans eux le client ne recevait que le
/// statut nu et affichait « Une erreur est survenue. Réessaie dans un instant.
/// (429) », qui ne dit ni ce qui s'est passé ni quand réessayer (#2178).
///
/// Le corps d'origine est conservé : on n'écrase jamais un `error` déjà posé
/// par mozaiklabs.
fn build_result(status: u16, text: &str, retry_after: Option<u64>) -> SupportResult {
    let mut value: Value = serde_json::from_str(text).unwrap_or_else(|_| json!({ "raw": text }));

    if (200..300).contains(&status) {
        return Ok(value);
    }

    if status == 429 {
        // Un corps non-objet (tableau, chaîne, `null`) ne peut pas porter les
        // champs : on le range sous `upstream` plutôt que de le perdre.
        if !value.is_object() {
            value = json!({ "upstream": value });
        }
        if let Some(obj) = value.as_object_mut() {
            obj.entry("error").or_insert_with(|| json!("rate_limited"));
            if let Some(secs) = retry_after {
                obj.insert("retry_after".to_string(), json!(secs));
            }
        }
    }

    Err((status, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn err_body(result: SupportResult) -> (u16, Value) {
        result.expect_err("un statut hors 2xx doit rendre une erreur")
    }

    #[test]
    fn retry_after_lit_les_delta_secondes() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("59"));
        assert_eq!(retry_after_secs(&h), Some(59));
    }

    #[test]
    fn retry_after_absent_rend_none() {
        assert_eq!(retry_after_secs(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_illisible_rend_none() {
        // Forme HTTP-date : non interprétée, et surtout jamais devinée.
        let mut h = HeaderMap::new();
        h.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after_secs(&h), None);
    }

    #[test]
    fn retry_after_zero_rend_none() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("0"));
        assert_eq!(retry_after_secs(&h), None);
    }

    #[test]
    fn retry_after_repli_sur_x_ratelimit_reset() {
        let mut h = HeaderMap::new();
        let futur = chrono::Utc::now().timestamp() + 120;
        h.insert(
            "x-ratelimit-reset",
            HeaderValue::from_str(&futur.to_string()).unwrap(),
        );
        let secs = retry_after_secs(&h).expect("un reset dans le futur donne un délai");
        // Bornes larges : la seconde peut tourner entre les deux appels.
        assert!((115..=120).contains(&secs), "délai inattendu : {secs}");
    }

    #[test]
    fn retry_after_reset_passe_rend_none() {
        let mut h = HeaderMap::new();
        let passe = chrono::Utc::now().timestamp() - 30;
        h.insert(
            "x-ratelimit-reset",
            HeaderValue::from_str(&passe.to_string()).unwrap(),
        );
        assert_eq!(retry_after_secs(&h), None);
    }

    #[test]
    fn un_429_avec_retry_after_porte_le_motif_et_le_delai() {
        // Corps réel du limiteur Laravel : un `message` anglais, rien d'autre.
        let (status, body) = err_body(build_result(
            429,
            r#"{"message":"Too Many Attempts."}"#,
            Some(3540),
        ));
        assert_eq!(status, 429);
        assert_eq!(body["error"], json!("rate_limited"));
        assert_eq!(body["retry_after"], json!(3540));
        // Le corps d'origine survit : le SAV doit pouvoir le lire.
        assert_eq!(body["message"], json!("Too Many Attempts."));
    }

    #[test]
    fn un_429_sans_en_tete_porte_le_motif_sans_delai() {
        let (status, body) = err_body(build_result(
            429,
            r#"{"message":"Too Many Attempts."}"#,
            None,
        ));
        assert_eq!(status, 429);
        assert_eq!(body["error"], json!("rate_limited"));
        assert!(
            body.get("retry_after").is_none(),
            "sans en-tête, aucun délai ne doit être inventé : {body}"
        );
    }

    #[test]
    fn un_429_a_corps_vide_reste_exploitable() {
        // mozaiklabs derrière un proxy peut ne rien renvoyer : le motif doit
        // quand même arriver au client.
        let (status, body) = err_body(build_result(429, "", Some(60)));
        assert_eq!(status, 429);
        assert_eq!(body["error"], json!("rate_limited"));
        assert_eq!(body["retry_after"], json!(60));
    }

    #[test]
    fn un_429_a_corps_non_objet_ne_perd_pas_l_amont() {
        let (_, body) = err_body(build_result(429, "[1,2]", Some(10)));
        assert_eq!(body["error"], json!("rate_limited"));
        assert_eq!(body["retry_after"], json!(10));
        assert_eq!(body["upstream"], json!([1, 2]));
    }

    #[test]
    fn un_429_conserve_le_motif_deja_pose_par_mozaiklabs() {
        let (_, body) = err_body(build_result(
            429,
            r#"{"error":"support_quota_daily"}"#,
            Some(60),
        ));
        assert_eq!(body["error"], json!("support_quota_daily"));
        assert_eq!(body["retry_after"], json!(60));
    }

    #[test]
    fn les_autres_statuts_ne_sont_pas_touches() {
        // 403 premium refusé : le corps repart tel quel, sans motif ajouté.
        let (status, body) = err_body(build_result(
            403,
            r#"{"message":"Premium only."}"#,
            Some(60),
        ));
        assert_eq!(status, 403);
        assert!(body.get("error").is_none(), "corps modifié : {body}");
        assert!(body.get("retry_after").is_none(), "corps modifié : {body}");
    }

    #[test]
    fn un_2xx_reste_un_succes() {
        let value = build_result(201, r#"{"ticket":{"id":7}}"#, None).expect("2xx = succès");
        assert_eq!(value["ticket"]["id"], json!(7));
    }
}
