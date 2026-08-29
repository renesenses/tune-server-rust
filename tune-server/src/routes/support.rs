//! Proxy HTTP vers l'API support premium de mozaiklabs.
//!
//! Le token OAuth premium (`mozaik_access_token`) vit en settings côté serveur ;
//! le client web ne l'a jamais → tout passe par ici. Voir
//! [`tune_core::cloud::support`]. Le gate premium autoritatif est côté
//! mozaiklabs (`auth.premium`) : un 403 y est renvoyé tel quel au client.

use axum::RequestExt;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::cloud::support;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// Limites alignées sur la validation Laravel (`StoreSupportTicketRequest`) :
/// au plus 5 fichiers, 50 Mo chacun, extensions autorisées ci-dessous.
const MAX_FILES: usize = 5;
const MAX_FILE_BYTES: usize = 50 * 1024 * 1024;
/// Plafond du corps multipart entrant (5×50 Mo + marge pour les champs texte).
/// Il DOIT surpasser le `DefaultBodyLimit` global (50 Mo) sinon un ticket avec
/// pièces jointes serait tronqué avant même d'atteindre le handler.
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const ALLOWED_EXT: &[&str] = &[
    "log", "txt", "zip", "json", "csv", "xml", "md", "png", "jpg", "jpeg",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/tickets",
            get(list)
                .post(create)
                // Relève le plafond pour ce seul endpoint : les pièces jointes
                // peuvent atteindre plusieurs dizaines de Mo.
                .layer(DefaultBodyLimit::max(MAX_TOTAL_BYTES)),
        )
        .route("/tickets/{id}", get(detail))
        .route("/tickets/{id}/reply", post(reply))
        // Dernier appel du support que le client web adressait encore en direct
        // à mozaiklabs.fr, clé de licence dans le corps (#2559).
        .route("/tickets/{id}/read", post(mark_read))
}

#[derive(Deserialize)]
struct CreateBody {
    subject: String,
    body: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
}

async fn list(State(state): State<AppState>) -> Response {
    let auth = match auth(&state) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    finish(support::list_tickets(&state.http_client, &auth).await)
}

/// Crée un ticket. Un seul endpoint pour deux formats : `application/json`
/// (sans pièce jointe, chemin historique) ou `multipart/form-data` (avec
/// `attachments[]`). Le format est choisi d'après le `Content-Type` entrant.
async fn create(State(state): State<AppState>, req: Request) -> Response {
    let auth = match auth(&state) {
        Ok(a) => a,
        Err(resp) => return resp,
    };

    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("multipart/form-data"))
        .unwrap_or(false);

    if is_multipart {
        create_multipart(state, auth, req).await
    } else {
        create_json(state, auth, req).await
    }
}

/// Chemin JSON historique — ticket sans pièce jointe.
async fn create_json(state: AppState, auth: support::SupportAuth, req: Request) -> Response {
    let payload = match req.extract::<Json<CreateBody>, _>().await {
        Ok(Json(p)) => p,
        Err(rej) => return rej.into_response(),
    };
    finish(
        support::create_ticket(
            &state.http_client,
            &auth,
            &payload.subject,
            &payload.body,
            payload.category.as_deref(),
        )
        .await,
    )
}

/// Chemin multipart — ticket avec pièces jointes. Valide nombre, taille et type
/// AVANT de relayer à mozaiklabs (message d'erreur clair sinon), puis transmet
/// le multipart tel quel avec la clé de licence / le token premium.
async fn create_multipart(state: AppState, auth: support::SupportAuth, req: Request) -> Response {
    let mut multipart = match req.extract::<Multipart, _>().await {
        Ok(m) => m,
        Err(rej) => return rej.into_response(),
    };

    let mut fields: Vec<(String, String)> = Vec::new();
    let mut files: Vec<support::AttachmentUpload> = Vec::new();
    let mut has_subject = false;
    let mut has_body = false;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return client_error("invalid_multipart", &e.to_string()),
        };

        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|s| s.to_string());
        let declared_ct = field.content_type().map(|s| s.to_string());

        match file_name {
            Some(fname) if !fname.is_empty() => {
                // Rejets AVANT de bufferiser le contenu : trop de fichiers, ou
                // extension non autorisée.
                if files.len() >= MAX_FILES {
                    return client_error(
                        "too_many_attachments",
                        "Trop de pièces jointes : 5 fichiers au maximum.",
                    );
                }
                let ext = ext_of(&fname);
                if !ext_allowed(&ext) {
                    return client_error(
                        "attachment_type",
                        &format!("Type de fichier non autorisé : « {fname} »."),
                    );
                }
                let bytes = match field.bytes().await {
                    Ok(b) => b,
                    Err(e) => return client_error("attachment_read", &e.to_string()),
                };
                if bytes.len() > MAX_FILE_BYTES {
                    return payload_too_large(&fname);
                }
                let content_type = declared_ct.unwrap_or_else(|| mime_for(&ext).to_string());
                files.push(support::AttachmentUpload {
                    file_name: fname,
                    content_type,
                    bytes: bytes.to_vec(),
                });
            }
            _ => {
                // Champ texte : liste blanche relayée telle quelle. On ignore
                // tune_version/platform d'un client (injectés côté serveur).
                let value = match field.text().await {
                    Ok(v) => v,
                    Err(e) => return client_error("invalid_field", &e.to_string()),
                };
                match name.as_str() {
                    "subject" => {
                        has_subject = !value.trim().is_empty();
                        fields.push((name, value));
                    }
                    "body" => {
                        has_body = !value.trim().is_empty();
                        fields.push((name, value));
                    }
                    "category" | "zone" | "system" | "logs" => fields.push((name, value)),
                    _ => {}
                }
            }
        }
    }

    if !has_subject || !has_body {
        return client_error(
            "missing_fields",
            "Le sujet et la description sont obligatoires.",
        );
    }

    finish(support::create_ticket_multipart(&state.http_client, &auth, fields, files).await)
}

/// 400 Bad Request avec un code machine + un message FR lisible par l'UI.
fn client_error(code: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// 413 Payload Too Large — une pièce jointe dépasse 50 Mo.
fn payload_too_large(file_name: &str) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({
            "error": "attachment_too_large",
            "message": format!("« {file_name} » dépasse la taille maximale de 50 Mo."),
        })),
    )
        .into_response()
}

/// Type MIME de repli quand le navigateur n'en déclare pas (rare).
fn mime_for(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "json" => "application/json",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "md" => "text/markdown",
        "log" | "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Extension en minuscules du nom de fichier (chaîne vide si aucune).
fn ext_of(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => String::new(),
    }
}

/// L'extension figure-t-elle dans la liste blanche (parité Laravel) ?
fn ext_allowed(ext: &str) -> bool {
    ALLOWED_EXT.contains(&ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_of_extracts_lowercased_extension() {
        assert_eq!(ext_of("capture.PNG"), "png");
        assert_eq!(ext_of("journal.tune.log"), "log");
        assert_eq!(ext_of("archive.tar.gz"), "gz");
        assert_eq!(ext_of("sans_extension"), "");
    }

    #[test]
    fn ext_allowed_matches_laravel_whitelist() {
        for ok in [
            "log", "txt", "zip", "json", "csv", "xml", "md", "png", "jpg", "jpeg",
        ] {
            assert!(ext_allowed(ok), "{ok} devrait être autorisé");
        }
        for bad in ["exe", "sh", "gz", "pdf", "bin", ""] {
            assert!(!ext_allowed(bad), "{bad} devrait être rejeté");
        }
    }

    #[test]
    fn mime_for_covers_whitelist_and_falls_back() {
        assert_eq!(mime_for("png"), "image/png");
        assert_eq!(mime_for("jpg"), "image/jpeg");
        assert_eq!(mime_for("jpeg"), "image/jpeg");
        assert_eq!(mime_for("json"), "application/json");
        assert_eq!(mime_for("log"), "text/plain");
        assert_eq!(mime_for("zip"), "application/zip");
        assert_eq!(mime_for("unknown"), "application/octet-stream");
    }

    #[test]
    fn limits_match_laravel_contract() {
        assert_eq!(MAX_FILES, 5);
        assert_eq!(MAX_FILE_BYTES, 50 * 1024 * 1024);
        // Le plafond du corps doit dépasser 5 fichiers pleins pour ne pas
        // tronquer un envoi légitime.
        assert!(MAX_TOTAL_BYTES > MAX_FILES * MAX_FILE_BYTES);
        // …et rester au-dessus du DefaultBodyLimit global (50 Mo).
        assert!(MAX_TOTAL_BYTES > 50 * 1024 * 1024);
    }
}

async fn detail(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let auth = match auth(&state) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    finish(support::get_ticket(&state.http_client, &auth, id).await)
}

async fn reply(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<ReplyBody>,
) -> Response {
    let auth = match auth(&state) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    finish(support::reply(&state.http_client, &auth, id, &payload.body).await)
}

/// Marque un fil comme lu. Aucun corps attendu : l'identité vient d'`auth()`,
/// jamais d'une clé de licence fournie par la page.
async fn mark_read(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let auth = match auth(&state) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    finish(support::mark_read(&state.http_client, &auth, id).await)
}

/// Résout l'auth vers mozaiklabs : token OAuth premium (SSO) en priorité, sinon
/// la clé de licence (premium par clé, sans SSO — la majorité des testeurs).
/// 412 seulement si NI l'un NI l'autre n'est disponible.
fn auth(state: &AppState) -> Result<support::SupportAuth, Response> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Chemin 1 : token OAuth premium (login SSO dans Tune).
    if let Some(token) = settings.get("mozaik_access_token").ok().flatten() {
        if !token.is_empty() {
            return Ok(support::SupportAuth::Bearer(token));
        }
    }

    // Chemin 2 : clé de licence. mozaiklabs vérifie la licence premium et
    // rattache le ticket au compte de l'e-mail de la licence.
    if let Some(key) = settings.get("license_key").ok().flatten() {
        if !key.is_empty() {
            let fingerprint = settings
                .get("hardware_fingerprint")
                .ok()
                .flatten()
                .filter(|f| !f.is_empty())
                .unwrap_or_else(tune_core::license::LicenseManager::hardware_fingerprint);
            return Ok(support::SupportAuth::License { key, fingerprint });
        }
    }

    Err((
        StatusCode::PRECONDITION_FAILED,
        Json(json!({
            "error": "not_connected",
            "message": "Connecte-toi à ton compte Tune ou active ta licence premium pour utiliser le support.",
        })),
    )
        .into_response())
}

/// Traduit le `SupportResult` en réponse HTTP, en préservant le status renvoyé
/// par mozaiklabs (401/403/422…).
///
/// Sur un 429, `tune_core::cloud::support` a déjà déposé `retry_after` dans le
/// corps ; on le réémet aussi en en-tête `Retry-After`, forme standard que
/// lisent les clients non web (#2178).
fn finish(result: support::SupportResult) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err((status, value)) => {
            let retry_after = value.get("retry_after").and_then(serde_json::Value::as_u64);
            let mut resp = (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(value),
            )
                .into_response();
            if let Some(secs) = retry_after {
                if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
                    resp.headers_mut().insert(header::RETRY_AFTER, v);
                }
            }
            resp
        }
    }
}
