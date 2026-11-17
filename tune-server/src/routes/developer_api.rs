use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::state::AppState;
use tune_http_types::panne_sql::OuDefautJournalise;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SETTINGS_KEY_API_KEYS: &str = "developer_api_keys";
const SETTINGS_KEY_WEBHOOKS: &str = "developer_webhooks";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevApiKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub scopes: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
struct CreateApiKeyRequest {
    name: String,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct CreateWebhookRequest {
    url: String,
    events: Vec<String>,
}

// ---------------------------------------------------------------------------
// Valid scopes & events
// ---------------------------------------------------------------------------

const VALID_SCOPES: &[&str] = &["read", "control", "write"];
const VALID_EVENTS: &[&str] = &[
    "track.started",
    "track.ended",
    "zone.changed",
    "volume.changed",
];

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{key_id}", delete(revoke_api_key))
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route("/webhooks/{id}", delete(delete_webhook))
        .route("/webhooks/test", post(test_webhooks))
}

// ---------------------------------------------------------------------------
// Helpers — settings persistence
// ---------------------------------------------------------------------------

fn load_api_keys(settings: &SettingsRepo) -> Result<Vec<DevApiKey>, String> {
    settings.get_json_list(SETTINGS_KEY_API_KEYS)
}

pub fn load_webhooks(settings: &SettingsRepo) -> Result<Vec<Webhook>, String> {
    settings.get_json_list(SETTINGS_KEY_WEBHOOKS)
}

/// Une panne de stockage se dit, elle ne se déguise pas en liste vide (#2795).
///
/// Le corps ne porte jamais la valeur stockée : `detail` vient des messages de
/// `SettingsRepo`, de `serde_json` (position seule) et du pilote SQL (jamais
/// les paramètres liés). Aucune clef d'API n'y transite — le test
/// `cles_developpeur_persistance.rs` le vérifie sur une base volontairement
/// corrompue avec une clef à l'intérieur.
fn panne_de_stockage(quoi: &str, erreur: String) -> axum::response::Response {
    // La valeur n'est PAS journalisée, seulement la cause.
    warn!(quoi, erreur = %erreur, "developer_api_stockage_en_echec");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "storage_failure",
            "detail": erreur,
        })),
    )
        .into_response()
}

/// L'origine d'une URL de webhook — schéma et hôte, rien de plus.
///
/// Une adresse de webhook est un secret en soi : chez Slack ou Discord, le
/// chemin (`/services/T…/B…/…`) EST le jeton, et quiconque l'a peut publier.
/// Le journal doit donc pouvoir nommer la destination sans la livrer. C'est la
/// même règle que pour les clefs d'API : jamais de secret dans une trace, même
/// tronqué.
pub fn origine_seule(url: &str) -> String {
    let (schema, reste) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        // Pas de schéma reconnaissable : on ne devine pas, on ne cite rien.
        None => return "(adresse illisible)".to_string(),
    };
    let hote = reste
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        // Un éventuel `user:motdepasse@` est lui aussi un secret.
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if hote.is_empty() {
        return "(adresse illisible)".to_string();
    }
    format!("{schema}://{hote}")
}

/// Generate a `tunedev_` prefixed key with 32 random hex chars.
fn generate_dev_key() -> String {
    let hex = uuid::Uuid::new_v4().to_string().replace('-', "");
    format!("tunedev_{hex}")
}

fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as ISO-8601 UTC using the `time` crate
    let dt = time::OffsetDateTime::from_unix_timestamp(now as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| format!("{now}"))
}

// ---------------------------------------------------------------------------
// API key endpoints
// ---------------------------------------------------------------------------

/// `GET /developer/api-keys` — list active developer API keys.
async fn list_api_keys(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let keys = match load_api_keys(&settings) {
        Ok(k) => k,
        Err(e) => return Ok(panne_de_stockage("lecture_cles", e)),
    };

    // Redact full keys in listing — show prefix only
    let redacted: Vec<Value> = keys
        .iter()
        .map(|k| {
            let preview = if k.key.len() > 12 {
                format!("{}...", &k.key[..12])
            } else {
                k.key.clone()
            };
            json!({
                "id": k.id,
                "name": k.name,
                "key_preview": preview,
                "scopes": k.scopes,
                "created_at": k.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "api_keys": redacted,
        "count": redacted.len(),
    }))
    .into_response())
}

/// `POST /developer/api-keys` — create a new developer API key.
async fn create_api_key(
    State(state): State<AppState>,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    // Validate scopes
    for scope in &body.scopes {
        if !VALID_SCOPES.contains(&scope.as_str()) {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid scope: {scope}"),
                    "valid_scopes": VALID_SCOPES,
                })),
            )
                .into_response());
        }
    }

    if body.name.trim().is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name is required"})),
        )
            .into_response());
    }

    if body.scopes.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one scope is required"})),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let new_key = DevApiKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name.trim().to_string(),
        key: generate_dev_key(),
        scopes: body.scopes,
        created_at: now_iso(),
    };

    // La lecture, l'ajout et l'écriture dans UNE transaction, un écrivain à la
    // fois : deux créations simultanées se retrouvent toutes les deux dans la
    // liste. Avant la #2795, la seconde réécrivait la liste lue avant que la
    // première ne s'y ajoute — et le client repartait avec une clef affichée
    // une fois, jamais persistée.
    let ajoutee = new_key.clone();
    if let Err(e) = settings.update_json_list::<DevApiKey, _, _>(SETTINGS_KEY_API_KEYS, move |k| {
        k.push(ajoutee);
        Ok(())
    }) {
        return Ok(panne_de_stockage("creation_cle", e));
    }

    // Journalisée APRÈS la persistance, et sans la clef : une trace ne doit
    // annoncer que ce qui est vrai, et jamais porter un secret.
    info!(name = %new_key.name, id = %new_key.id, "developer_api_key_created");

    let response = json!({
        "id": new_key.id,
        "name": new_key.name,
        "key": new_key.key,
        "scopes": new_key.scopes,
        "created_at": new_key.created_at,
    });

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// `DELETE /developer/api-keys/{key_id}` — revoke a developer API key.
async fn revoke_api_key(
    State(state): State<AppState>,
    Path(key_id): Path<String>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let cible = key_id.clone();
    let trouvee =
        match settings.update_json_list::<DevApiKey, _, _>(SETTINGS_KEY_API_KEYS, move |keys| {
            let avant = keys.len();
            keys.retain(|k| k.id != cible);
            Ok(keys.len() != avant)
        }) {
            Ok(t) => t,
            // Une révocation annoncée sans effet est pire que le refus : la
            // clef reste valable et son propriétaire la croit morte.
            Err(e) => return Ok(panne_de_stockage("revocation_cle", e)),
        };

    if !trouvee {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "api key not found"})),
        )
            .into_response());
    }

    info!(key_id = %key_id, "developer_api_key_revoked");

    Ok(Json(json!({"ok": true, "revoked": key_id})).into_response())
}

// ---------------------------------------------------------------------------
// Webhook endpoints
// ---------------------------------------------------------------------------

/// `GET /developer/webhooks` — list registered webhooks.
async fn list_webhooks(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let hooks = match load_webhooks(&settings) {
        Ok(h) => h,
        Err(e) => return Ok(panne_de_stockage("lecture_webhooks", e)),
    };

    Ok(Json(json!({
        "webhooks": hooks,
        "count": hooks.len(),
    }))
    .into_response())
}

/// `POST /developer/webhooks` — register a webhook.
async fn create_webhook(
    State(state): State<AppState>,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    // Validate URL
    let url = body.url.trim().to_string();
    if url.is_empty() || (!url.starts_with("http://") && !url.starts_with("https://")) {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "url must be a valid http(s) URL"})),
        )
            .into_response());
    }
    // Sécurité : un webhook ne vise jamais le serveur lui-même ni le réseau
    // local — le POST partirait de 127.0.0.1 et passerait pour un appel local.
    if let Err(motif) = juger_destination_webhook(&url, MomentWebhook::Creation).await {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "webhook_destination_refused",
                "message": format!("webhook destination refused: {motif}"),
            })),
        )
            .into_response());
    }

    // Validate events
    for ev in &body.events {
        if !VALID_EVENTS.contains(&ev.as_str()) {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid event: {ev}"),
                    "valid_events": VALID_EVENTS,
                })),
            )
                .into_response());
        }
    }

    if body.events.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one event is required"})),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let webhook = Webhook {
        id: uuid::Uuid::new_v4().to_string(),
        url,
        events: body.events,
        created_at: now_iso(),
    };

    let ajoute = webhook.clone();
    if let Err(e) = settings.update_json_list::<Webhook, _, _>(SETTINGS_KEY_WEBHOOKS, move |h| {
        h.push(ajoute);
        Ok(())
    }) {
        return Ok(panne_de_stockage("creation_webhook", e));
    }

    info!(
        id = %webhook.id,
        origine = %origine_seule(&webhook.url),
        "developer_webhook_registered"
    );

    let response = json!({
        "id": webhook.id,
        "url": webhook.url,
        "events": webhook.events,
        "created_at": webhook.created_at,
    });

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// `DELETE /developer/webhooks/{id}` — remove a webhook.
async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let cible = id.clone();
    let trouve =
        match settings.update_json_list::<Webhook, _, _>(SETTINGS_KEY_WEBHOOKS, move |hooks| {
            let avant = hooks.len();
            hooks.retain(|h| h.id != cible);
            Ok(hooks.len() != avant)
        }) {
            Ok(t) => t,
            Err(e) => return Ok(panne_de_stockage("suppression_webhook", e)),
        };

    if !trouve {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "webhook not found"})),
        )
            .into_response());
    }

    info!(webhook_id = %id, "developer_webhook_removed");

    Ok(Json(json!({"ok": true, "removed": id})).into_response())
}

/// `POST /developer/webhooks/test` — send a test event to all webhooks.
async fn test_webhooks(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let hooks = match load_webhooks(&settings) {
        Ok(h) => h,
        // « 0 envoyé, aucun webhook enregistré » sur une base en panne
        // enverrait l'utilisateur reconfigurer ce qui existe déjà.
        Err(e) => return Ok(panne_de_stockage("lecture_webhooks", e)),
    };

    if hooks.is_empty() {
        return Ok(Json(json!({
            "sent": 0,
            "message": "no webhooks registered",
        }))
        .into_response());
    }

    let test_payload = json!({
        "event": "test",
        "timestamp": now_iso(),
        "data": {
            "message": "This is a test webhook event from Tune Developer API",
        },
    });

    let mut sent = 0u32;
    let mut errors = Vec::new();

    for hook in &hooks {
        let result = match juger_destination_webhook(&hook.url, MomentWebhook::Envoi).await {
            Err(motif) => {
                warn!(url = %tune_core::confidentialite::masquer_url(&hook.url), motif, "webhook_destination_refusee");
                errors.push(json!({
                    "webhook_id": hook.id,
                    "url": hook.url,
                    "error": format!("destination refused ({motif})"),
                }));
                continue;
            }
            Ok(destination) => envoyer_webhook(&destination, &test_payload).await,
        };

        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    sent += 1;
                } else {
                    errors.push(json!({
                        "webhook_id": hook.id,
                        "url": hook.url,
                        "status": status,
                    }));
                }
            }
            Err(e) => {
                errors.push(json!({
                    "webhook_id": hook.id,
                    "url": hook.url,
                    "error": e.to_string(),
                }));
            }
        }
    }

    Ok(Json(json!({
        "sent": sent,
        "errors": errors,
        "total": hooks.len(),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Webhook dispatcher — background task
// ---------------------------------------------------------------------------

/// Spawn a background task that listens for playback events and dispatches
/// matching events to registered webhook URLs.  Fire-and-forget: webhook
/// failures never block playback.
/// Pourquoi une destination de webhook est refusée, ou `None` si elle passe :
/// hôte littéral interdit (bouclage, non spécifié, privé, lien local,
/// multidiffusion…), nom local (`localhost`, `.local`, `.lan`, `.internal`,
/// `.home.arpa`), ou nom qui RÉSOUT vers une adresse interdite. Vérifié à la
/// création ET avant chaque envoi (le DNS peut changer entre les deux).
/// Quand la destination d'un webhook est jugée.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MomentWebhook {
    /// À l'enregistrement : un nom qui ne résout pas encore est admis (DNS
    /// absent, domaine réservé aux tests) — il sera rejugé à l'envoi.
    Creation,
    /// Avant chaque envoi : un nom qui ne résout pas n'est pas appelé.
    Envoi,
}

/// Une destination de webhook jugée bonne : l'URL telle qu'enregistrée, son
/// hôte, et l'adresse ÉPINGLÉE quand l'hôte est un nom — le client n'en
/// résoudra pas une autre entre le jugement et la connexion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DestinationWebhook {
    pub url: String,
    pub hote: String,
    pub epinglee: Option<std::net::SocketAddr>,
}

/// Politique des destinations (décision du 15/11 : « refuser la machine,
/// autoriser le LAN ») : refusés le bouclage, l'adresse non spécifiée, le lien
/// local (dont les métadonnées cloud 169.254.169.254), les adresses propres de
/// la machine, le multicast et le broadcast ; admis le reste, dont RFC 1918 et
/// ULA (Home Assistant, n8n sur le réseau local).
pub(crate) fn adresse_webhook_interdite(
    ip: std::net::IpAddr,
    machine: &[std::net::IpAddr],
) -> bool {
    use std::net::IpAddr;
    fn canonique(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(v6)),
            v4 => v4,
        }
    }
    let ip = canonique(ip);
    if machine.iter().any(|m| canonique(*m) == ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.is_multicast()
        }
    }
}

/// Une adresse réseau NUE : `hôte` ou `hôte:port` (nom, IPv4, IPv6 entre
/// crochets), rien d'autre. Une intégration qui compose son URL par
/// `http://{hôte}/chemin` ne peut alors viser que SON chemin : ni `/`, ni `?`,
/// ni `#`, ni `@`, ni espace ou `%` pour déplacer la requête ailleurs.
pub(crate) fn adresse_reseau_nue(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn nom_d_hote_local(nom: &str) -> bool {
    nom == "localhost"
        || [
            ".localhost",
            ".local",
            ".lan",
            ".internal",
            ".home.arpa",
            ".home",
        ]
        .iter()
        .any(|s| nom.ends_with(s))
}

/// Juge une destination dont la résolution est déjà faite (pure, testable).
/// `resolution` : `Err(())` si le nom ne résout pas (ou pas encore).
pub(crate) fn juger_destination_resolue(
    url: &str,
    resolution: Result<&[std::net::SocketAddr], ()>,
    moment: MomentWebhook,
    machine: &[std::net::IpAddr],
) -> Result<DestinationWebhook, &'static str> {
    juger_destination_resolue_avec(url, resolution, moment, machine, NomsLocaux::Refuses)
}

/// Les noms locaux (`.local`, `.lan`…) : refusés pour un webhook, admis pour
/// une intégration du réseau local (Home Assistant répond sur
/// `homeassistant.local`) — leurs adresses restent jugées. `localhost` est
/// toujours refusé.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NomsLocaux {
    Refuses,
    Admis,
}

pub(crate) fn juger_destination_resolue_avec(
    url: &str,
    resolution: Result<&[std::net::SocketAddr], ()>,
    moment: MomentWebhook,
    machine: &[std::net::IpAddr],
    noms_locaux: NomsLocaux,
) -> Result<DestinationWebhook, &'static str> {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Err("unparseable url");
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("scheme must be http or https");
    }
    let Some(hote) = parsed.host_str() else {
        return Err("missing host");
    };
    let hote = hote.to_string();
    // Littéral IPv4, ou IPv6 entre crochets : jugé tel quel, rien à épingler.
    let litteral = hote.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = litteral.parse::<std::net::IpAddr>() {
        if adresse_webhook_interdite(ip, machine) {
            return Err("forbidden address");
        }
        return Ok(DestinationWebhook {
            url: url.to_string(),
            hote,
            epinglee: None,
        });
    }
    let nom = hote.trim_end_matches('.').to_ascii_lowercase();
    let refuse = match noms_locaux {
        NomsLocaux::Refuses => nom_d_hote_local(&nom),
        NomsLocaux::Admis => nom == "localhost" || nom.ends_with(".localhost"),
    };
    if refuse {
        return Err("local host name");
    }
    match resolution {
        Ok(adresses) if !adresses.is_empty() => {
            if adresses
                .iter()
                .any(|a| adresse_webhook_interdite(a.ip(), machine))
            {
                return Err("resolves to a forbidden address");
            }
            Ok(DestinationWebhook {
                url: url.to_string(),
                hote,
                epinglee: Some(adresses[0]),
            })
        }
        _ if moment == MomentWebhook::Creation => Ok(DestinationWebhook {
            url: url.to_string(),
            hote,
            epinglee: None,
        }),
        _ => Err("host does not resolve"),
    }
}

/// Juge une destination : résout le nom s'il y a lieu, puis
/// [`juger_destination_resolue`] avec les adresses propres de la machine.
pub(crate) async fn juger_destination_webhook(
    url: &str,
    moment: MomentWebhook,
) -> Result<DestinationWebhook, &'static str> {
    juger_destination_avec(url, moment, NomsLocaux::Refuses).await
}

/// Comme [`juger_destination_webhook`], en choisissant le sort des noms locaux.
pub(crate) async fn juger_destination_avec(
    url: &str,
    moment: MomentWebhook,
    noms_locaux: NomsLocaux,
) -> Result<DestinationWebhook, &'static str> {
    let machine = tune_core::discovery::adresses_de_la_machine();
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Err("unparseable url");
    };
    let hote = parsed.host_str().unwrap_or_default().to_string();
    let litteral = hote.trim_start_matches('[').trim_end_matches(']');
    if hote.is_empty() || litteral.parse::<std::net::IpAddr>().is_ok() {
        return juger_destination_resolue_avec(url, Err(()), moment, &machine, noms_locaux);
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let adresses: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host((hote.as_str(), port)).await {
            Ok(it) => it.collect(),
            Err(_) => Vec::new(),
        };
    let resolution = if adresses.is_empty() {
        Err(())
    } else {
        Ok(adresses.as_slice())
    };
    juger_destination_resolue_avec(url, resolution, moment, &machine, noms_locaux)
}

/// Le client dédié aux webhooks : aucune redirection suivie (un 3xx vers la
/// machine ne mène nulle part), délai court, et l'hôte épinglé sur l'adresse
/// jugée — une seconde résolution ne peut pas renvoyer ailleurs.
pub(crate) fn client_webhook(destination: &DestinationWebhook) -> reqwest::Result<reqwest::Client> {
    // Depuis le constructeur partagé (rustls, délais) : la couture HTTP du
    // dépôt l'exige, et la build FFI n'a pas de vérificateur TLS de plateforme.
    let mut b = tune_core::http::client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Tune-Webhook/2.0");
    if let Some(adresse) = destination.epinglee {
        b = b.resolve(&destination.hote, adresse);
    }
    b.build()
}

/// Envoie la charge utile à une destination jugée bonne.
async fn envoyer_webhook(
    destination: &DestinationWebhook,
    corps: &Value,
) -> reqwest::Result<reqwest::Response> {
    client_webhook(destination)?
        .post(&destination.url)
        .header("Content-Type", "application/json")
        .json(corps)
        .send()
        .await
}

pub fn spawn_webhook_dispatcher(state: &AppState) {
    let playback = state.playback.clone();
    let backend = state.backend.clone();
    let event_bus = state.event_bus.clone();

    // Clone before moving into first spawn
    let backend2 = backend.clone();

    // Subscribe to playback events (broadcast channel)
    let mut playback_rx = playback.subscribe();

    tokio::spawn(async move {
        info!("webhook_dispatcher_started");
        loop {
            match playback_rx.recv().await {
                Ok(event) => {
                    // Map playback event names to webhook event names
                    let webhook_event = match event.event.as_str() {
                        "started" => "track.started",
                        "ended" | "finished" => "track.ended",
                        "volume_changed" => "volume.changed",
                        _ => continue,
                    };

                    let settings = SettingsRepo::with_backend(backend.clone());
                    // Ici, et ici seulement, la liste vide reste acceptable :
                    // le distributeur est « au mieux » et ne doit pas mourir
                    // sur une panne passagère. Mais elle ne sera plus
                    // silencieuse — et surtout, ce chemin n'ÉCRIT rien, donc
                    // il ne peut pas remplacer les webhooks par `[]`.
                    let hooks = load_webhooks(&settings).ou_defaut_journalise();

                    if hooks.is_empty() {
                        continue;
                    }

                    let matching: Vec<&Webhook> = hooks
                        .iter()
                        .filter(|h| h.events.contains(&webhook_event.to_string()))
                        .collect();

                    if matching.is_empty() {
                        continue;
                    }

                    let payload = json!({
                        "event": webhook_event,
                        "zone_id": event.zone_id,
                        "timestamp": now_iso(),
                        "data": event.data,
                    });

                    for hook in matching {
                        let url = hook.url.clone();
                        let body = payload.clone();
                        // Fire-and-forget — don't block on webhook delivery
                        tokio::spawn(async move {
                            let destination =
                                match juger_destination_webhook(&url, MomentWebhook::Envoi).await {
                                    Ok(d) => d,
                                    Err(motif) => {
                                        warn!(url = %tune_core::confidentialite::masquer_url(&url), motif, "webhook_destination_refusee");
                                        return;
                                    }
                                };
                            let result = envoyer_webhook(&destination, &body).await;

                            if let Err(e) = result {
                                warn!(
                                    origine = %origine_seule(&url),
                                    error = %e,
                                    "webhook_delivery_failed"
                                );
                            }
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "webhook_dispatcher_lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("webhook_dispatcher_channel_closed");
                    break;
                }
            }
        }
    });

    // Also subscribe to the general EventBus for zone events
    let mut bus_rx = event_bus.subscribe();

    tokio::spawn(async move {
        loop {
            match bus_rx.recv().await {
                Ok(event) => {
                    let webhook_event = match event.event_type.as_str() {
                        "zone.created" | "zone.deleted" | "zone.updated" => "zone.changed",
                        _ => continue,
                    };

                    let settings = SettingsRepo::with_backend(backend2.clone());
                    let hooks = load_webhooks(&settings).ou_defaut_journalise();

                    let matching: Vec<&Webhook> = hooks
                        .iter()
                        .filter(|h| h.events.contains(&webhook_event.to_string()))
                        .collect();

                    if matching.is_empty() {
                        continue;
                    }

                    let payload = json!({
                        "event": webhook_event,
                        "timestamp": now_iso(),
                        "data": event.data,
                    });

                    for hook in matching {
                        let url = hook.url.clone();
                        let body = payload.clone();
                        tokio::spawn(async move {
                            let destination =
                                match juger_destination_webhook(&url, MomentWebhook::Envoi).await {
                                    Ok(d) => d,
                                    Err(motif) => {
                                        warn!(url = %tune_core::confidentialite::masquer_url(&url), motif, "webhook_destination_refusee");
                                        return;
                                    }
                                };
                            let result = envoyer_webhook(&destination, &body).await;

                            if let Err(e) = result {
                                warn!(
                                    origine = %origine_seule(&url),
                                    error = %e,
                                    "webhook_delivery_failed"
                                );
                            }
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "webhook_bus_dispatcher_lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("webhook_bus_dispatcher_closed");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod destinations_webhook_tests {
    use super::{
        DestinationWebhook, MomentWebhook, adresse_webhook_interdite, client_webhook,
        juger_destination_resolue,
    };
    use std::net::{IpAddr, SocketAddr};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn la_machine_est_refusee_le_lan_est_admis() {
        let machine = [ip("192.168.1.18"), ip("fd00::18"), ip("127.0.0.1")];
        for ko in [
            "127.0.0.1",
            "127.0.0.2",
            "::1",
            "::ffff:127.0.0.1",
            "0.0.0.0",
            "::",
            "169.254.169.254",
            "169.254.1.1",
            "fe80::1",
            "224.0.0.1",
            "255.255.255.255",
            "ff02::1",
            "192.168.1.18",
            "fd00::18",
            "::ffff:192.168.1.18",
        ] {
            assert!(adresse_webhook_interdite(ip(ko), &machine), "{ko}");
        }
        for ok in [
            "192.168.1.10",
            "10.0.0.5",
            "172.16.0.2",
            "fd00::10",
            "203.0.113.5",
            "::ffff:192.168.1.10",
        ] {
            assert!(!adresse_webhook_interdite(ip(ok), &machine), "{ok}");
        }
    }

    #[test]
    fn un_litteral_du_lan_passe_et_la_machine_est_refusee() {
        let machine = [ip("192.168.1.18")];
        let ok = juger_destination_resolue(
            "http://192.168.1.10:8123/api/webhook/x",
            Err(()),
            MomentWebhook::Envoi,
            &machine,
        )
        .unwrap();
        assert_eq!(ok.epinglee, None);
        assert_eq!(ok.hote, "192.168.1.10");
        for url in [
            "http://127.0.0.1:8888/api/v1/appliance/reboot",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8888/x",
            "http://0.0.0.0/x",
            "http://192.168.1.18:8888/x",
            "http://localhost:8888/x",
            "http://tune.local/x",
            "ftp://192.168.1.10/x",
        ] {
            assert!(
                juger_destination_resolue(url, Err(()), MomentWebhook::Envoi, &machine).is_err(),
                "{url}"
            );
            assert!(
                juger_destination_resolue(url, Err(()), MomentWebhook::Creation, &machine).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn un_nom_qui_resout_en_bouclage_est_refuse_a_l_envoi_et_epingle_sinon() {
        let machine = [ip("192.168.1.18")];
        let bouclage: &[SocketAddr] = &["127.0.0.1:80".parse().unwrap()];
        assert_eq!(
            juger_destination_resolue(
                "http://hook.exemple.test/h",
                Ok(bouclage),
                MomentWebhook::Envoi,
                &machine
            ),
            Err("resolves to a forbidden address")
        );
        // Une seule adresse interdite parmi d'autres suffit.
        let melange: &[SocketAddr] = &[
            "192.168.1.10:80".parse().unwrap(),
            "127.0.0.1:80".parse().unwrap(),
        ];
        assert!(
            juger_destination_resolue(
                "http://hook.exemple.test/h",
                Ok(melange),
                MomentWebhook::Envoi,
                &machine
            )
            .is_err()
        );
        let lan: &[SocketAddr] = &["192.168.1.10:80".parse().unwrap()];
        let d = juger_destination_resolue(
            "http://hook.exemple.test/h",
            Ok(lan),
            MomentWebhook::Envoi,
            &machine,
        )
        .unwrap();
        assert_eq!(d.epinglee, Some("192.168.1.10:80".parse().unwrap()));
        // Pas encore résolu : admis à la création, refusé à l'envoi.
        assert!(
            juger_destination_resolue(
                "http://hook.exemple.test/h",
                Err(()),
                MomentWebhook::Creation,
                &machine
            )
            .is_ok()
        );
        assert_eq!(
            juger_destination_resolue(
                "http://hook.exemple.test/h",
                Err(()),
                MomentWebhook::Envoi,
                &machine
            ),
            Err("host does not resolve")
        );
    }

    /// Le client des webhooks ne suit pas un 307 : un serveur du LAN qui
    /// renverrait vers 127.0.0.1 n'atteint pas la machine. Le serveur témoin
    /// écoute en bouclage — on exerce ici le CLIENT, pas la politique
    /// d'adresse (testée au-dessus), d'où la destination posée à la main.
    #[tokio::test]
    async fn un_307_vers_le_bouclage_n_est_pas_suivi() {
        use axum::{Router, response::IntoResponse, routing::post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let touches = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = listener.local_addr().unwrap();
        let cible = format!("http://127.0.0.1:{}/cible", adresse.port());
        let t = touches.clone();
        let app = Router::new()
            .route(
                "/depart",
                post(move || {
                    let cible = cible.clone();
                    async move {
                        (
                            axum::http::StatusCode::TEMPORARY_REDIRECT,
                            [(axum::http::header::LOCATION, cible)],
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/cible",
                post(move || {
                    t.fetch_add(1, Ordering::SeqCst);
                    async { "touche" }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let destination = DestinationWebhook {
            url: format!("http://127.0.0.1:{}/depart", adresse.port()),
            hote: "127.0.0.1".into(),
            epinglee: None,
        };
        let reponse = client_webhook(&destination)
            .unwrap()
            .post(&destination.url)
            .json(&serde_json::json!({"event": "test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(reponse.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            touches.load(Ordering::SeqCst),
            0,
            "la cible du 307 a été appelée"
        );
    }

    /// L'adresse épinglée est bien celle que le client utilise : un nom
    /// quelconque, épinglé sur le serveur témoin, y arrive sans DNS.
    #[tokio::test]
    async fn l_adresse_epinglee_est_celle_du_client() {
        use axum::{Router, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = listener.local_addr().unwrap();
        let app = Router::new().route("/h", post(|| async { "ok" }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let destination = DestinationWebhook {
            url: format!("http://hook.exemple.test:{}/h", adresse.port()),
            hote: "hook.exemple.test".into(),
            epinglee: Some(adresse),
        };
        let reponse = client_webhook(&destination)
            .unwrap()
            .post(&destination.url)
            .send()
            .await
            .unwrap();
        assert_eq!(reponse.status(), reqwest::StatusCode::OK);
    }
}

#[cfg(test)]
mod destinations_integration_tests {
    use super::{MomentWebhook, NomsLocaux, adresse_reseau_nue, juger_destination_resolue_avec};
    use std::net::SocketAddr;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn une_integration_admet_les_noms_locaux_mais_juge_leurs_adresses() {
        let machine = ["192.168.1.18".parse().unwrap()];
        let url = "http://homeassistant.local:8123/api/states";
        let lan = [a("192.168.1.20:8123")];
        assert!(
            juger_destination_resolue_avec(
                url,
                Ok(&lan),
                MomentWebhook::Envoi,
                &machine,
                NomsLocaux::Admis
            )
            .is_ok()
        );
        // Le même nom pour un webhook reste refusé.
        assert!(
            juger_destination_resolue_avec(
                url,
                Ok(&lan),
                MomentWebhook::Envoi,
                &machine,
                NomsLocaux::Refuses
            )
            .is_err()
        );
        for interdite in [a("127.0.0.1:8123"), a("192.168.1.18:8123")] {
            assert!(
                juger_destination_resolue_avec(
                    url,
                    Ok(&[interdite]),
                    MomentWebhook::Envoi,
                    &machine,
                    NomsLocaux::Admis
                )
                .is_err(),
                "{interdite}"
            );
        }
        assert!(
            juger_destination_resolue_avec(
                "http://localhost:8123",
                Err(()),
                MomentWebhook::Creation,
                &machine,
                NomsLocaux::Admis
            )
            .is_err()
        );
    }

    #[test]
    fn une_adresse_reseau_nue_n_a_ni_chemin_ni_requete() {
        for ok in [
            "localhost:1705",
            "192.168.1.30",
            "[fd00::4]:80",
            "snap-server.lan",
        ] {
            assert!(adresse_reseau_nue(ok), "{ok}");
        }
        for ko in [
            "",
            "127.0.0.1:8888/api/v1/appliance/shutdown?",
            "h#",
            "u@127.0.0.1",
            "h ost",
            "h%2f",
            "h\\x",
        ] {
            assert!(!adresse_reseau_nue(ko), "{ko}");
        }
    }
}
