//! Proxy HTTP vers l'API support premium de mozaiklabs.
//!
//! Le token OAuth premium (`mozaik_access_token`) vit en settings côté serveur ;
//! le client web ne l'a jamais → tout passe par ici. Voir
//! [`tune_core::cloud::support`]. Le gate premium autoritatif est côté
//! mozaiklabs (`auth.premium`) : un 403 y est renvoyé tel quel au client.

use axum::RequestExt;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

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

/// Corps JSON d'un ticket sans pièce jointe manuelle.
///
/// ⚠️ `zone`, `system` et `logs` doivent y figurer : le client les envoie sur
/// CE chemin comme sur le multipart (case « joindre les journaux », cochée par
/// défaut). Tant qu'ils manquaient ici, serde les jetait en silence, mozaiklabs
/// ne recevait aucun `logs`, ne créait donc aucun `diagnostic.md` — et le ticket
/// arrivait quand même en 201, le client annonçant « envoyé ».
#[derive(Deserialize)]
struct CreateBody {
    subject: String,
    body: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    zone: Option<String>,
    #[serde(default)]
    system: Option<Value>,
    #[serde(default)]
    logs: Option<String>,
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
}

async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let relay = match relay(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    finish(
        support::list_tickets(&state.http_client, &relay).await,
        &headers,
    )
}

/// Crée un ticket. Un seul endpoint pour deux formats : `application/json`
/// (sans pièce jointe, chemin historique) ou `multipart/form-data` (avec
/// `attachments[]`). Le format est choisi d'après le `Content-Type` entrant.
async fn create(State(state): State<AppState>, req: Request) -> Response {
    let relay = match relay(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("multipart/form-data"))
        .unwrap_or(false);

    // Les en-têtes sont copiés AVANT l'extraction du corps, qui consomme la
    // requête : sans eux, `Accept-Language` serait perdu et un 429 repartirait
    // en français quelle que soit la langue de l'interface (#2178).
    let headers = req.headers().clone();

    if is_multipart {
        create_multipart(state, relay, req, headers).await
    } else {
        create_json(state, relay, req, headers).await
    }
}

/// Chemin JSON historique — ticket sans pièce jointe.
async fn create_json(
    state: AppState,
    relay: support::SupportRelay,
    req: Request,
    headers: HeaderMap,
) -> Response {
    let payload = match req.extract::<Json<CreateBody>, _>().await {
        Ok(Json(p)) => p,
        Err(rej) => return rej.into_response(),
    };
    finish(
        support::create_ticket(
            &state.http_client,
            &relay,
            &support::NewTicket {
                subject: payload.subject,
                body: payload.body,
                category: payload.category,
                zone: payload.zone,
                system: payload.system,
                logs: payload.logs,
            },
        )
        .await,
        &headers,
    )
}

/// Chemin multipart — ticket avec pièces jointes. Valide nombre, taille et type
/// AVANT de relayer à mozaiklabs (message d'erreur clair sinon), puis transmet
/// le multipart tel quel avec la clé de licence / le token premium.
async fn create_multipart(
    state: AppState,
    relay: support::SupportRelay,
    req: Request,
    headers: HeaderMap,
) -> Response {
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

    finish(
        support::create_ticket_multipart(&state.http_client, &relay, fields, files).await,
        &headers,
    )
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

    /// Le corps que le client web envoie réellement quand il n'y a AUCUNE pièce
    /// jointe manuelle (`SupportView.submitTicket`, chemin JSON) : il porte
    /// `logs` et `system`. `CreateBody` doit les retenir — serde jette en
    /// silence tout champ absent de la structure, et c'est ainsi que le
    /// diagnostic disparaissait sans une seule ligne de journal.
    #[test]
    fn le_corps_json_du_client_conserve_le_diagnostic() {
        let brut = r##"{
            "subject": "Coupure DLNA",
            "body": "Le salon s'arrête au bout de dix secondes.",
            "category": "bug",
            "zone": "Salon",
            "logs": "# Tune Bug Report\n\nERROR dlna_stall",
            "system": { "os": "linux", "zones": 3 }
        }"##;

        let parsed: CreateBody = serde_json::from_str(brut).expect("corps client valide");

        assert_eq!(
            parsed.logs.as_deref(),
            Some("# Tune Bug Report\n\nERROR dlna_stall"),
            "le diagnostic doit survivre à la désérialisation"
        );
        assert_eq!(parsed.zone.as_deref(), Some("Salon"));
        assert_eq!(
            parsed.system.as_ref().map(|s| s["os"].clone()),
            Some(json!("linux"))
        );
    }

    /// Un client plus ancien n'envoie ni `logs` ni `system` : le ticket doit
    /// s'ouvrir quand même (rétro-compatibilité, #1073).
    #[test]
    fn un_corps_sans_diagnostic_reste_accepte() {
        let parsed: CreateBody = serde_json::from_str(r#"{"subject":"Question","body":"DSD ?"}"#)
            .expect("corps minimal");
        assert!(parsed.logs.is_none());
        assert!(parsed.system.is_none());
        assert!(parsed.category.is_none());
    }

    // -----------------------------------------------------------------------
    // Le 429 du relais support : un message exploitable, dans la bonne langue,
    // et jamais un délai inventé (#2178).
    // -----------------------------------------------------------------------

    fn accept_language(tag: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_LANGUAGE, tag.parse().unwrap());
        h
    }

    /// Corps tel que `tune_core::cloud::support::build_result` le produit sur un
    /// 429 : le texte anglais du limiteur Laravel, plus le motif et le délai
    /// posés par le relais.
    fn corps_429(retry_after: Option<u64>) -> Value {
        let mut v = json!({ "message": "Too Many Attempts.", "error": "rate_limited" });
        if let Some(secs) = retry_after {
            v["retry_after"] = json!(secs);
        }
        v
    }

    async fn reponse(
        status: u16,
        body: Value,
        headers: &HeaderMap,
    ) -> (StatusCode, Value, Option<String>) {
        let resp = finish(Err((status, body)), headers);
        let code = resp.status();
        let retry = resp
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_string());
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (code, serde_json::from_slice(&octets).unwrap(), retry)
    }

    #[test]
    fn minutes_a_attendre_arrondit_au_superieur_sans_jamais_zero() {
        // Jamais zéro : « réessaie dans 0 min » renverrait l'utilisateur trop
        // tôt, donc sur un nouveau 429.
        assert_eq!(minutes_a_attendre(1), 1);
        assert_eq!(minutes_a_attendre(59), 1);
        assert_eq!(minutes_a_attendre(60), 1);
        assert_eq!(minutes_a_attendre(61), 2);
        assert_eq!(minutes_a_attendre(3540), 59);
    }

    #[tokio::test]
    async fn un_429_porte_un_message_localise_et_le_delai() {
        let (code, body, retry) = reponse(429, corps_429(Some(90)), &accept_language("fr")).await;

        assert_eq!(
            code,
            StatusCode::TOO_MANY_REQUESTS,
            "le statut ne change pas"
        );
        // Le défaut d'origine : le seul texte disponible était anglais et
        // technique. Il ne doit plus être ce que le client affiche.
        assert_ne!(body["message"], json!("Too Many Attempts."));
        let message = body["message"].as_str().expect("un message texte");
        assert!(
            message.contains("2 min"),
            "le délai doit être dit à l'utilisateur : {message}"
        );
        assert!(
            !message.contains("{minutes}"),
            "l'interpolation n'a pas eu lieu : {message}"
        );
        // Le contrat machine et le délai exact survivent.
        assert_eq!(body["error"], json!("rate_limited"));
        assert_eq!(body["retry_after"], json!(90));
        assert_eq!(retry.as_deref(), Some("90"));
        // Le texte amont est déplacé, pas perdu.
        assert_eq!(body["upstream_message"], json!("Too Many Attempts."));
    }

    #[tokio::test]
    async fn un_429_suit_la_langue_de_l_interface() {
        let (_, fr, _) = reponse(429, corps_429(Some(60)), &accept_language("fr")).await;
        let (_, en, _) =
            reponse(429, corps_429(Some(60)), &accept_language("en-US,en;q=0.9")).await;
        let (_, de, _) = reponse(429, corps_429(Some(60)), &accept_language("de")).await;

        assert_ne!(fr["message"], en["message"]);
        assert_ne!(fr["message"], de["message"]);
        // Sans en-tête, français — la langue par défaut de l'application.
        let (_, defaut, _) = reponse(429, corps_429(Some(60)), &HeaderMap::new()).await;
        assert_eq!(defaut["message"], fr["message"]);
    }

    #[tokio::test]
    async fn un_429_sans_delai_ne_l_invente_pas() {
        let (code, body, retry) = reponse(429, corps_429(None), &accept_language("fr")).await;

        assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
        let message = body["message"].as_str().expect("un message texte");
        // Le message nomme la cause — c'est ce qui manquait — mais ne chiffre
        // aucune attente que mozaiklabs n'a pas annoncée.
        assert_ne!(message, "Too Many Attempts.");
        assert!(
            !message.chars().any(|c| c.is_ascii_digit()),
            "aucun délai ne doit être fabriqué : {message}"
        );
        assert!(body.get("retry_after").is_none());
        assert_eq!(retry, None, "pas d'en-tête Retry-After sans délai connu");
    }

    #[tokio::test]
    async fn les_dix_langues_disent_la_limite() {
        for lang in crate::i18n::SUPPORTED {
            let (_, avec, _) = reponse(429, corps_429(Some(120)), &accept_language(lang)).await;
            let message = avec["message"].as_str().unwrap();
            assert_ne!(
                message, "support.tropDeRequetesDelai",
                "traduction manquante pour {lang}"
            );
            assert!(message.contains('2'), "délai absent en {lang} : {message}");
            assert!(!message.contains("{minutes}"), "{lang} : {message}");

            let (_, sans, _) = reponse(429, corps_429(None), &accept_language(lang)).await;
            assert_ne!(
                sans["message"].as_str().unwrap(),
                "support.tropDeRequetes",
                "traduction manquante pour {lang}"
            );
        }
    }

    #[tokio::test]
    async fn les_autres_statuts_gardent_leur_texte() {
        // 403 premium refusé : mozaiklabs a déjà écrit un message français
        // exploitable, on n'y touche pas.
        let amont = json!({
            "error": "premium_required",
            "message": "Le support prioritaire est réservé à Tune Premium",
        });
        let (code, body, _) = reponse(403, amont.clone(), &accept_language("fr")).await;

        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(body, amont, "corps modifié hors 429 : {body}");
    }
}

async fn detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let relay = match relay(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    finish(
        support::get_ticket(&state.http_client, &relay, id).await,
        &headers,
    )
}

async fn reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(payload): Json<ReplyBody>,
) -> Response {
    let relay = match relay(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    finish(
        support::reply(&state.http_client, &relay, id, &payload.body).await,
        &headers,
    )
}

/// Marque un fil comme lu. Aucun corps attendu : l'identité vient d'`auth()`,
/// jamais d'une clé de licence fournie par la page.
async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let relay = match relay(&state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    finish(
        support::mark_read(&state.http_client, &relay, id).await,
        &headers,
    )
}

/// Résout la cible du relais : l'adresse du nuage ET l'auth vers mozaiklabs —
/// token OAuth premium (SSO) en priorité, sinon la clé de licence (premium par
/// clé, sans SSO — la majorité des testeurs). 412 seulement si NI l'un NI
/// l'autre n'est disponible.
///
/// L'adresse vient du réglage `mozaik_base_url`, comme pour les pochettes
/// communautaires (`routes/cloud.rs`), les signalements de métadonnées
/// (`routes/library/reports.rs`) et le magasin de greffons. Le support était la
/// seule porte du nuage à l'ignorer : il partait toujours vers
/// `https://mozaiklabs.fr`, et c'est pour cela que le transport du diagnostic et
/// des pièces jointes — ce que le miroir forum annonce ensuite (#2856) — n'était
/// vérifié par aucun test.
fn relay(state: &AppState) -> Result<support::SupportRelay, Response> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();

    // Chemin 1 : token OAuth premium (login SSO dans Tune).
    if let Some(token) = settings.get("mozaik_access_token").ok().flatten() {
        if !token.is_empty() {
            return Ok(support::SupportRelay::new(
                base_url.as_deref(),
                support::SupportAuth::Bearer(token),
            ));
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
            return Ok(support::SupportRelay::new(
                base_url.as_deref(),
                support::SupportAuth::License { key, fingerprint },
            ));
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

/// Minutes à attendre, déduites des secondes annoncées par mozaiklabs.
///
/// L'arrondi se fait vers le HAUT, et jamais à zéro : renvoyer l'utilisateur
/// « dans 0 min » le ferait revenir trop tôt et reprendre un 429. Le délai
/// exact en secondes n'est pas perdu pour autant — il reste dans le corps
/// (`retry_after`) et dans l'en-tête `Retry-After`, pour qui programme.
fn minutes_a_attendre(secondes: u64) -> u64 {
    secondes.div_ceil(60).max(1)
}

/// Remplace le texte d'un 429 par un message localisé et exploitable.
///
/// Le limiteur de Laravel ne sait dire qu'une chose, en anglais et sans
/// contexte : `{"message":"Too Many Attempts."}`. Un client qui affiche
/// `message` montrait donc ce texte-là, et celui qui ne le lit pas retombait
/// sur son message générique — « Une erreur est survenue (429) », qui ne dit ni
/// ce qui s'est passé ni quand réessayer (#2178).
///
/// On écrit ici, dans la langue de l'interface (`Accept-Language`, comme la
/// porte des clés Radio France), ce que le serveur sait réellement : la limite
/// vient du service distant, et — quand mozaiklabs l'annonce — le délai avant
/// nouvelle tentative. **Aucun délai n'est inventé** : sans en-tête
/// exploitable, le message le tait au lieu de le supposer.
///
/// Le texte amont n'est pas perdu : il est déplacé sous `upstream_message`,
/// pour le SAV et pour le diagnostic. Le code machine `error` (posé par
/// `tune_core::cloud::support`) n'est pas touché : les clients qui programment
/// contre `rate_limited` gardent leur contrat.
fn localiser_limite(value: &mut Value, headers: &HeaderMap, retry_after: Option<u64>) {
    let lang = crate::i18n::lang_from_header(headers);
    let message = match retry_after {
        Some(secondes) => crate::i18n::t(&lang, "support.tropDeRequetesDelai")
            .replace("{minutes}", &minutes_a_attendre(secondes).to_string()),
        None => crate::i18n::t(&lang, "support.tropDeRequetes"),
    };

    // `build_result` garantit un objet sur un 429, mais on ne parie pas dessus.
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if let Some(amont) = obj.insert("message".to_string(), json!(message)) {
        obj.entry("upstream_message").or_insert(amont);
    }
}

/// Traduit le `SupportResult` en réponse HTTP, en préservant le status renvoyé
/// par mozaiklabs (401/403/422…).
///
/// Sur un 429, `tune_core::cloud::support` a déjà déposé `retry_after` dans le
/// corps ; on le réémet aussi en en-tête `Retry-After`, forme standard que
/// lisent les clients non web, et on remplace le texte anglais du limiteur par
/// un message localisé (voir [`localiser_limite`]). Le **statut reste 429** :
/// il est juste, et les clients déployés le reçoivent déjà — seul le corps
/// change (#2178).
fn finish(result: support::SupportResult, headers: &HeaderMap) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err((status, mut value)) => {
            let retry_after = value.get("retry_after").and_then(Value::as_u64);
            if status == 429 {
                localiser_limite(&mut value, headers, retry_after);
            }
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
