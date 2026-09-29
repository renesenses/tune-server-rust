//! Installer un greffon audio natif depuis le catalogue de mozaiklabs.
//!
//! `POST /api/v1/audio-plugins/{id}/install-from-catalog` (administrateur) :
//!
//! 1. les gardes de l'emplacement, AVANT tout appel réseau
//!    ([`crate::native_audio::check_slot`]) : identifiant admissible, nom
//!    libre, Premium. Un compte Free est refusé sans rien demander au site ;
//! 2. la fiche du paquet : `GET {mozaik_base_url}/api/v1/audio-plugins/{id}/package
//!    ?target=<triplet de l'hôte>&tune_version=<version>`, authentifiée comme le
//!    support (token SSO, puis clé de licence si le compte SSO est refusé). Le
//!    site rend `{id, version, target, url, sha256, signature, size}` pour une
//!    licence Premium valide ;
//! 3. le téléchargement de `url`, borné à la taille maximale d'une archive ;
//! 4. la somme SHA-256 annoncée, puis la signature minisign contre les clés de
//!    confiance ([`crate::native_audio::trusted_keys`] : la clé des greffons de
//!    Mozaiklabs, intégrée, plus celles de l'opérateur) ;
//! 5. l'installation par le chemin commun à l'envoi direct
//!    ([`crate::native_audio::install_package`]), active au prochain démarrage.
//!
//! Le site ne fait que DISTRIBUER : la confiance vient de la signature, jamais
//! du canal. Un paquet dont la signature n'est pas de confiance est refusé
//! même si le site l'a servi.
//!
//! Chaque échec porte un code `error` stable pour le client : voir [`Refus`].
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tune_core::cloud::support::SupportAuth;
use tune_core::db::settings_repo::SettingsRepo;

use crate::{auth::RequireAdmin, state::AppState};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";
/// La fiche est un petit JSON : un site qui ne répond pas en 20 s est en panne.
const DELAI_FICHE: Duration = Duration::from_secs(20);
/// Le paquet peut peser quelques mégaoctets sur une ligne lente.
const DELAI_PAQUET: Duration = Duration::from_secs(300);

/// La fiche d'un paquet, telle que le catalogue la rend.
#[derive(Debug, Deserialize)]
struct FichePaquet {
    id: String,
    version: String,
    target: String,
    url: String,
    sha256: String,
    signature: String,
}

/// Les refus de l'installation depuis le catalogue. Le code `error` est le
/// contrat du client ; `detail` est pour le journal et l'opérateur.
#[derive(Debug, PartialEq)]
enum Refus {
    /// Ni token SSO ni clé de licence : rien à présenter au site (412).
    NonConnecte,
    /// Le site ne reconnaît pas de licence Premium valide (402).
    PremiumRefuseParLeSite(String),
    /// Le catalogue ne connaît pas ce greffon (404).
    AbsentDuCatalogue,
    /// Le greffon existe, mais pas pour cette plateforme (404).
    PasDePaquetPourLaPlateforme,
    /// Le site limite ce serveur (503), avec son délai s'il le donne.
    Limite(Option<u64>),
    /// Réseau, délai, erreur 5xx ou réponse illisible du site (502).
    Injoignable(String),
    /// La fiche ne décrit pas le paquet demandé (502).
    FicheIncoherente(String),
    /// Le téléchargement du paquet a échoué (502).
    Telechargement(String),
    /// Le paquet dépasse la taille maximale d'une archive (502).
    TropGros,
    /// Les octets reçus ne sont pas ceux que la fiche annonce (502).
    SommeFausse,
    /// La signature n'est pas celle d'une clé de confiance (400).
    SignatureInvalide(String),
    /// L'hôte refuse le paquet pour une autre raison (400).
    Installation(String),
}

impl Refus {
    fn en_reponse(&self, id: &str, cible: &str) -> Response {
        let (status, code, detail) = match self {
            Refus::NonConnecte => (
                StatusCode::PRECONDITION_FAILED,
                "not_connected",
                "Connecte-toi à ton compte Tune ou active ta licence Premium pour installer ce greffon."
                    .to_string(),
            ),
            Refus::PremiumRefuseParLeSite(d) => {
                (StatusCode::PAYMENT_REQUIRED, "premium_required", d.clone())
            }
            Refus::AbsentDuCatalogue => (
                StatusCode::NOT_FOUND,
                "plugin_not_in_catalog",
                format!("{id} n'est pas dans le catalogue"),
            ),
            Refus::PasDePaquetPourLaPlateforme => (
                StatusCode::NOT_FOUND,
                "no_package_for_target",
                format!("aucun paquet de {id} pour {cible}"),
            ),
            Refus::Limite(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "catalog_rate_limited",
                "le catalogue limite ce serveur ; réessayer plus tard".to_string(),
            ),
            Refus::Injoignable(d) => (StatusCode::BAD_GATEWAY, "catalog_unreachable", d.clone()),
            Refus::FicheIncoherente(d) => {
                (StatusCode::BAD_GATEWAY, "catalog_invalid_reply", d.clone())
            }
            Refus::Telechargement(d) => {
                (StatusCode::BAD_GATEWAY, "package_download_failed", d.clone())
            }
            Refus::TropGros => (
                StatusCode::BAD_GATEWAY,
                "package_too_large",
                "le paquet dépasse la taille maximale d'une archive".to_string(),
            ),
            Refus::SommeFausse => (
                StatusCode::BAD_GATEWAY,
                "package_checksum_mismatch",
                "le paquet reçu ne correspond pas à la somme annoncée".to_string(),
            ),
            Refus::SignatureInvalide(d) => {
                (StatusCode::BAD_REQUEST, "signature_invalid", d.clone())
            }
            Refus::Installation(d) => (StatusCode::BAD_REQUEST, "native_plugin_refused", d.clone()),
        };
        let mut corps = json!({"error": code, "detail": detail, "plugin": id, "target": cible});
        if let Refus::Limite(Some(secondes)) = self {
            corps["retry_after_seconds"] = json!(secondes);
        }
        (status, Json(corps)).into_response()
    }
}

fn racine(state: &AppState) -> String {
    crate::routes::support::base_url(state)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim()
        .trim_end_matches('/')
        .to_string()
}

/// Demande la fiche du paquet. Essaie chaque identifiant dans l'ordre et ne
/// passe au suivant que sur un refus d'authentification (401/403) : un compte
/// SSO sans Premium ne doit pas masquer une clé de licence Premium.
async fn demander_la_fiche(
    http: &reqwest::Client,
    base: &str,
    id: &str,
    cible: &str,
    identifiants: &[SupportAuth],
) -> Result<FichePaquet, Refus> {
    let url = format!(
        "{base}/api/v1/audio-plugins/{}/package",
        urlencoding::encode(id)
    );
    let mut dernier_refus = Refus::NonConnecte;
    for auth in identifiants {
        let requete = auth
            .apply(http.get(&url))
            .query(&[("target", cible), ("tune_version", tune_core::version())])
            .header("Accept", "application/json")
            .timeout(DELAI_FICHE);
        let reponse = requete
            .send()
            .await
            .map_err(|e| Refus::Injoignable(e.to_string()))?;
        let status = reponse.status().as_u16();
        let retry_after = reponse
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let corps: Value = reponse.json().await.unwrap_or(Value::Null);
        let code = corps["error"].as_str().unwrap_or_default();
        match status {
            200 => {
                return serde_json::from_value(corps)
                    .map_err(|e| Refus::FicheIncoherente(format!("fiche illisible : {e}")));
            }
            401 | 403 => {
                let message = corps["message"]
                    .as_str()
                    .or(corps["error"].as_str())
                    .unwrap_or("licence Premium non reconnue par le catalogue");
                dernier_refus = Refus::PremiumRefuseParLeSite(message.to_string());
                continue;
            }
            404 if code == "no_package_for_target" => {
                return Err(Refus::PasDePaquetPourLaPlateforme);
            }
            404 => return Err(Refus::AbsentDuCatalogue),
            429 => return Err(Refus::Limite(retry_after)),
            _ => {
                return Err(Refus::Injoignable(format!(
                    "le catalogue a répondu {status}"
                )));
            }
        }
    }
    Err(dernier_refus)
}

/// Télécharge le paquet sans jamais garder plus que la taille maximale d'une
/// archive en mémoire.
async fn telecharger(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, Refus> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| Refus::FicheIncoherente(format!("url du paquet illisible : {e}")))?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(Refus::FicheIncoherente(
            "url du paquet hors http(s)".to_string(),
        ));
    }
    let max = tune_plugin_native::package::MAX_ARCHIVE;
    let mut reponse = http
        .get(parsed)
        .timeout(DELAI_PAQUET)
        .send()
        .await
        .map_err(|e| Refus::Telechargement(e.to_string()))?;
    if !reponse.status().is_success() {
        return Err(Refus::Telechargement(format!(
            "le paquet a répondu {}",
            reponse.status().as_u16()
        )));
    }
    if reponse.content_length().is_some_and(|n| n > max) {
        return Err(Refus::TropGros);
    }
    let mut octets = Vec::new();
    while let Some(morceau) = reponse
        .chunk()
        .await
        .map_err(|e| Refus::Telechargement(e.to_string()))?
    {
        if octets.len() as u64 + morceau.len() as u64 > max {
            return Err(Refus::TropGros);
        }
        octets.extend_from_slice(&morceau);
    }
    Ok(octets)
}

/// La signature est-elle celle d'une clé de confiance ? Même vérification que
/// l'installation (`tune_plugin_native::package`), faite ICI pour rendre un
/// code d'erreur propre ; l'installation la refait de toute façon.
fn verifier_la_signature(octets: &[u8], signature: &str, cles: &[String]) -> Result<(), Refus> {
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|e| Refus::SignatureInvalide(format!("signature illisible : {e}")))?;
    if cles
        .iter()
        .filter_map(|cle| minisign_verify::PublicKey::from_base64(cle).ok())
        .any(|cle| cle.verify(octets, &signature, false).is_ok())
    {
        Ok(())
    } else {
        Err(Refus::SignatureInvalide(
            "la signature du paquet n'est pas celle d'une clé de confiance".to_string(),
        ))
    }
}

/// La fiche du paquet de cette plateforme, vérifiée : elle décrit bien le
/// greffon et le triplet demandés.
async fn fiche_de_la_plateforme(
    state: &AppState,
    id: &str,
    cible: &str,
) -> Result<FichePaquet, Refus> {
    let identifiants = crate::routes::support::identifiants_mozaiklabs(
        &SettingsRepo::with_backend(state.backend.clone()),
    );
    if identifiants.is_empty() {
        return Err(Refus::NonConnecte);
    }
    let fiche =
        demander_la_fiche(&state.http_client, &racine(state), id, cible, &identifiants).await?;
    if fiche.id != id || fiche.target != cible {
        return Err(Refus::FicheIncoherente(format!(
            "fiche pour {}/{} au lieu de {id}/{cible}",
            fiche.id, fiche.target
        )));
    }
    Ok(fiche)
}

/// `candidate` est-elle PLUS RÉCENTE que `installee` ? Comparaison des
/// segments numériques (`0.10.0` > `0.9.3`) ; un segment non numérique se
/// compare comme texte. Deux versions égales ne sont pas une mise à jour.
fn plus_recente(candidate: &str, installee: &str) -> bool {
    let segments = |v: &str| -> Vec<String> {
        v.trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(str::to_string)
            .collect()
    };
    let (a, b) = (segments(candidate), segments(installee));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).map(String::as_str).unwrap_or("0");
        let y = b.get(i).map(String::as_str).unwrap_or("0");
        let ordre = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ordre != std::cmp::Ordering::Equal {
            return ordre == std::cmp::Ordering::Greater;
        }
    }
    false
}

/// `GET /api/v1/audio-plugins/{id}/catalog` (administrateur) : ce que le
/// catalogue publie pour la plateforme de CE serveur, comparé à ce qui est
/// installé. Ne télécharge rien, n'installe rien.
///
/// `available: false` + `reason: "no_package_for_target"` (200) quand le
/// greffon existe au catalogue mais pas pour ce triplet : la carte l'affiche
/// grisée, sans bouton. `update_available` n'est vrai que pour un greffon
/// installé depuis le catalogue dont la version est plus ancienne ; la mise à
/// jour passe par `install-from-catalog`, jamais d'elle-même.
pub async fn catalog_status(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = crate::native_audio::check_slot(&state, &id).await {
        return response;
    }
    let cible = tune_plugin_native::package::host_target();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let installe = settings
        .get(&format!("plugin_{id}_installed"))
        .is_ok_and(|v| v.as_deref() == Some("true"));
    let version_installee = settings.get(&format!("plugin_{id}_version")).ok().flatten();
    match fiche_de_la_plateforme(&state, &id, cible).await {
        Ok(fiche) => {
            let mise_a_jour = installe
                && version_installee
                    .as_deref()
                    .is_some_and(|v| plus_recente(&fiche.version, v));
            Json(json!({
                "id": id,
                "target": cible,
                "available": true,
                "latest_version": fiche.version,
                "installed": installe,
                "installed_version": version_installee,
                "update_available": mise_a_jour,
            }))
            .into_response()
        }
        Err(Refus::PasDePaquetPourLaPlateforme) => Json(json!({
            "id": id,
            "target": cible,
            "available": false,
            "reason": "no_package_for_target",
            "latest_version": null,
            "installed": installe,
            "installed_version": version_installee,
            "update_available": false,
        }))
        .into_response(),
        Err(refus) => refus.en_reponse(&id, cible),
    }
}

async fn installer(state: &AppState, id: &str, tiers: bool, cible: &str) -> Result<Value, Refus> {
    let fiche = fiche_de_la_plateforme(state, id, cible).await?;
    let octets = telecharger(&state.http_client, &fiche.url).await?;
    let somme = format!("{:x}", Sha256::digest(&octets));
    if !somme.eq_ignore_ascii_case(fiche.sha256.trim()) {
        return Err(Refus::SommeFausse);
    }
    let cles = crate::native_audio::trusted_keys().map_err(Refus::Installation)?;
    verifier_la_signature(&octets, &fiche.signature, &cles)?;
    let paquet = crate::native_audio::install_package(
        state,
        id,
        tiers,
        octets,
        fiche.signature.clone(),
        Some(&fiche.version),
    )
    .await
    .map_err(Refus::Installation)?;
    Ok(json!({
        "id": id,
        "installed": true,
        "restart_required": true,
        "target": paquet.target,
        "version": fiche.version,
        "source": "catalog",
    }))
}

/// `POST /api/v1/audio-plugins/{id}/install-from-catalog`
pub async fn install_from_catalog(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let tiers = match crate::native_audio::check_slot(&state, &id).await {
        Ok(tiers) => tiers,
        Err(response) => return response,
    };
    let cible = tune_plugin_native::package::host_target();
    match installer(&state, &id, tiers, cible).await {
        Ok(corps) => {
            tracing::info!(plugin = %id, target = cible, "audio_plugin_installed_from_catalog");
            Json(corps).into_response()
        }
        Err(refus) => {
            tracing::warn!(plugin = %id, target = cible, refus = ?refus, "audio_plugin_catalog_install_refused");
            refus.en_reponse(&id, cible)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Contre un catalogue FACTICE (un `mozaiklabs` local, `mozaik_base_url`),
    //! avec un paquet réellement construit par le SDK et signé par une clé de
    //! test publique, ajoutée à la confiance par `TUNE_AUDIO_PLUGIN_PUBLIC_KEY`.
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// Clé de test, publiquement connue — jamais une clé de production.
    const GRAINE_DE_TEST: [u8; 32] = [11; 32];
    const ID_CLE_DE_TEST: [u8; 8] = [5; 8];

    fn paire_de_test() -> ring::signature::Ed25519KeyPair {
        ring::signature::Ed25519KeyPair::from_seed_unchecked(&GRAINE_DE_TEST).unwrap()
    }

    fn cle_publique_de_test() -> String {
        use ring::signature::KeyPair;
        let mut pk = b"Ed".to_vec();
        pk.extend(ID_CLE_DE_TEST);
        pk.extend(paire_de_test().public_key().as_ref());
        STANDARD.encode(pk)
    }

    /// Signature minisign détachée (prédigest BLAKE2b-512, « ED »).
    fn signer(octets: &[u8]) -> String {
        use blake2::Digest as _;
        let paire = paire_de_test();
        let signature = paire.sign(&blake2::Blake2b512::digest(octets));
        let mut sig = b"ED".to_vec();
        sig.extend(ID_CLE_DE_TEST);
        sig.extend(signature.as_ref());
        let mut globale = signature.as_ref().to_vec();
        globale.extend(b"paquet de test");
        format!(
            "untrusted comment: test\n{}\ntrusted comment: paquet de test\n{}\n",
            STANDARD.encode(sig),
            STANDARD.encode(paire.sign(&globale).as_ref())
        )
    }

    /// Un vrai paquet du SDK, pour la plateforme de l'hôte.
    fn paquet(id: &str) -> Vec<u8> {
        let manifest: tune_plugin_sdk::manifest::Manifest = serde_json::from_value(json!({
            "id": id, "sdk": {"major": 0, "minor": 1}, "kind": "dsp", "config_version": 1,
            "entitlement": id, "distribution": "source",
            "capabilities": [{"id": "audio-process", "version": {"major": 0, "minor": 1}, "required": true}]
        }))
        .unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let binaire = dossier.path().join("libgreffon.so");
        std::fs::write(&binaire, b"pas une vraie bibliotheque").unwrap();
        tune_plugin_native::package::pack(
            manifest,
            &binaire,
            tune_plugin_native::package::host_target(),
            &BTreeMap::new(),
        )
        .unwrap()
    }

    /// Ce que le catalogue factice rend à la demande de fiche.
    #[derive(Clone)]
    enum Catalogue {
        /// Une fiche pour ce paquet, avec cette signature et cette somme.
        Fiche {
            octets: Vec<u8>,
            signature: String,
            sha256: String,
        },
        /// Un refus du site : status et corps.
        Refus(u16, Value),
    }

    #[derive(Clone)]
    struct Site {
        catalogue: Catalogue,
        /// Les en-têtes d'authentification reçus, dans l'ordre.
        vus: Arc<Mutex<Vec<String>>>,
        /// Refuse le Bearer (compte SSO sans Premium) et accepte la clé.
        bearer_refuse: bool,
        base: Arc<Mutex<String>>,
        /// La version que la fiche annonce ; modifiable en cours de test.
        version: Arc<Mutex<String>>,
    }

    async fn site_factice(catalogue: Catalogue, bearer_refuse: bool) -> Site {
        let site = Site {
            catalogue,
            vus: Arc::default(),
            bearer_refuse,
            base: Arc::default(),
            version: Arc::new(Mutex::new("0.3.1".to_string())),
        };
        let pour_la_fiche = site.clone();
        let pour_le_paquet = site.clone();
        let app = axum::Router::new()
            .route(
                "/api/v1/audio-plugins/{id}/package",
                axum::routing::get(
                    move |Path(id): Path<String>,
                          axum::extract::Query(q): axum::extract::Query<
                        BTreeMap<String, String>,
                    >,
                          headers: axum::http::HeaderMap| {
                        let site = pour_la_fiche.clone();
                        async move {
                            let bearer = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string);
                            let cle = headers
                                .get("x-license-key")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string);
                            site.vus.lock().unwrap().push(
                                bearer
                                    .clone()
                                    .or(cle.clone().map(|c| format!("cle {c}")))
                                    .unwrap_or_default(),
                            );
                            if bearer.is_some() && site.bearer_refuse {
                                return (
                                    StatusCode::FORBIDDEN,
                                    Json(json!({"error": "premium_required"})),
                                )
                                    .into_response();
                            }
                            match site.catalogue {
                                Catalogue::Refus(status, corps) => {
                                    (StatusCode::from_u16(status).unwrap(), Json(corps))
                                        .into_response()
                                }
                                Catalogue::Fiche {
                                    signature, sha256, ..
                                } => {
                                    let base = site.base.lock().unwrap().clone();
                                    let version = site.version.lock().unwrap().clone();
                                    Json(json!({
                                        "id": id,
                                        "version": version,
                                        "target": q.get("target").cloned().unwrap_or_default(),
                                        "url": format!("{base}/paquets/{id}"),
                                        "sha256": sha256,
                                        "signature": signature,
                                        "size": 1,
                                    }))
                                    .into_response()
                                }
                            }
                        }
                    },
                ),
            )
            .route(
                "/paquets/{id}",
                axum::routing::get(move || {
                    let site = pour_le_paquet.clone();
                    async move {
                        match site.catalogue {
                            Catalogue::Fiche { octets, .. } => octets.into_response(),
                            Catalogue::Refus(..) => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }),
            );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });
        *site.base.lock().unwrap() = format!("http://{adresse}");
        site
    }

    fn fiche_signee(id: &str) -> Catalogue {
        let octets = paquet(id);
        Catalogue::Fiche {
            signature: signer(&octets),
            sha256: format!("{:x}", Sha256::digest(&octets)),
            octets,
        }
    }

    /// La clé de test s'AJOUTE à celle de Mozaiklabs, comme le ferait un
    /// opérateur. Posée une fois pour tout le binaire de test, avant tout
    /// `trusted_keys()` de ce module ; aucun autre test ne lit cette variable.
    fn confiance_de_test() {
        static POSEE: std::sync::Once = std::sync::Once::new();
        POSEE.call_once(|| {
            // Safety : seule écriture de cette variable dans le processus.
            unsafe { std::env::set_var("TUNE_AUDIO_PLUGIN_PUBLIC_KEY", cle_publique_de_test()) };
        });
    }

    async fn serveur(base: &str, premium: bool, bearer: bool) -> (AppState, axum::Router) {
        crate::premium_audio_plugins::tests::dossier_de_donnees_jetable();
        confiance_de_test();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings.set("mozaik_base_url", base).unwrap();
        settings
            .set("license_key", "TUNE-TEST-0000-0000-0000")
            .unwrap();
        settings
            .set("hardware_fingerprint", "empreinte-test")
            .unwrap();
        if bearer {
            settings.set("mozaik_access_token", "jeton-sso").unwrap();
        }
        if premium {
            state
                .license
                .update_from_server(tune_core::license::Tier::Premium, None)
                .await;
        }
        let app = crate::routes::router_with_plugins(state.clone(), vec![]);
        (state, app)
    }

    async fn installer_par_la_route(app: &axum::Router, id: &str) -> (StatusCode, Value) {
        let reponse = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/audio-plugins/{id}/install-from-catalog"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn la_cle_de_test_est_lisible_par_minisign() {
        let octets = b"octets";
        let sig = minisign_verify::Signature::decode(&signer(octets)).unwrap();
        let cle = minisign_verify::PublicKey::from_base64(&cle_publique_de_test()).unwrap();
        assert!(cle.verify(octets, &sig, false).is_ok());
        assert!(cle.verify(b"autres octets", &sig, false).is_err());
    }

    /// Le chemin nominal : fiche, téléchargement, somme, signature, et le
    /// greffon est installé pour le prochain démarrage, version retenue.
    #[tokio::test]
    async fn premium_installe_depuis_le_catalogue() {
        let id = "catalogue-essai-nominal";
        let site = site_factice(fiche_signee(id), false).await;
        let base = site.base.lock().unwrap().clone();
        let (state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::OK, "{corps}");
        assert_eq!(corps["installed"], true, "{corps}");
        assert_eq!(corps["restart_required"], true, "{corps}");
        assert_eq!(corps["version"], "0.3.1", "{corps}");
        assert_eq!(corps["target"], tune_plugin_native::package::host_target());
        let settings = SettingsRepo::with_backend(state.backend.clone());
        assert_eq!(
            settings
                .get(&format!("plugin_{id}_installed"))
                .unwrap()
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            settings
                .get(&format!("plugin_{id}_version"))
                .unwrap()
                .as_deref(),
            Some("0.3.1")
        );
        assert!(
            tune_plugin_native::package::version_directory(&crate::native_audio::root(), id)
                .is_ok(),
            "aucune version active sur le disque"
        );
        assert_eq!(
            site.vus.lock().unwrap().as_slice(),
            ["cle TUNE-TEST-0000-0000-0000"]
        );
        // L'état des greffons publie la version du catalogue.
        let reponse = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/audio-plugins")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let etat: Value = serde_json::from_slice(&octets).unwrap();
        let fiche = etat["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("{id} absent de l'état : {etat}"));
        assert_eq!(fiche["version"], "0.3.1", "{fiche}");
        assert_eq!(fiche["third_party"], true, "{fiche}");
    }

    /// Contre-épreuve : Free est refusé AVANT tout appel au site.
    #[tokio::test]
    async fn free_refuse_sans_appeler_le_site() {
        let id = "catalogue-essai-free";
        let site = site_factice(fiche_signee(id), false).await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, false, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{corps}");
        assert!(site.vus.lock().unwrap().is_empty(), "le site a été appelé");
        assert!(!crate::native_audio::is_third_party(id), "installé en Free");
    }

    /// Contre-épreuve : une signature d'une autre clé est refusée, rien
    /// n'est installé.
    #[tokio::test]
    async fn signature_fausse_refusee() {
        let id = "catalogue-essai-signature";
        let octets = paquet(id);
        let autre = paquet("catalogue-autre-paquet");
        let site = site_factice(
            Catalogue::Fiche {
                // Signature valide… d'AUTRES octets.
                signature: signer(&autre),
                sha256: format!("{:x}", Sha256::digest(&octets)),
                octets,
            },
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
        assert_eq!(corps["error"], "signature_invalid", "{corps}");
        assert!(
            !crate::native_audio::is_third_party(id),
            "installé malgré la signature"
        );
        let settings = SettingsRepo::with_backend(state.backend.clone());
        assert!(
            settings
                .get(&format!("plugin_{id}_installed"))
                .unwrap()
                .is_none()
        );
    }

    /// Une signature illisible est aussi un refus de signature.
    #[tokio::test]
    async fn signature_illisible_refusee() {
        let id = "catalogue-essai-illisible";
        let octets = paquet(id);
        let site = site_factice(
            Catalogue::Fiche {
                signature: "pas une signature".into(),
                sha256: format!("{:x}", Sha256::digest(&octets)),
                octets,
            },
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{corps}");
        assert_eq!(corps["error"], "signature_invalid", "{corps}");
    }

    #[tokio::test]
    async fn somme_fausse_refusee() {
        let id = "catalogue-essai-somme";
        let octets = paquet(id);
        let site = site_factice(
            Catalogue::Fiche {
                signature: signer(&octets),
                sha256: "0".repeat(64),
                octets,
            },
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{corps}");
        assert_eq!(corps["error"], "package_checksum_mismatch", "{corps}");
        assert!(!crate::native_audio::is_third_party(id));
    }

    /// Le site refuse la licence (Premium expiré côté site, par exemple).
    #[tokio::test]
    async fn refus_premium_du_site() {
        let id = "catalogue-essai-refus-site";
        let site = site_factice(
            Catalogue::Refus(
                403,
                json!({"error": "premium_required", "message": "licence expirée"}),
            ),
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{corps}");
        assert_eq!(corps["error"], "premium_required", "{corps}");
        assert_eq!(corps["detail"], "licence expirée", "{corps}");
    }

    /// Un compte SSO sans Premium ne masque pas une clé de licence Premium.
    #[tokio::test]
    async fn sso_refuse_puis_cle_acceptee() {
        let id = "catalogue-essai-sso";
        let site = site_factice(fiche_signee(id), true).await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, true).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::OK, "{corps}");
        assert_eq!(
            site.vus.lock().unwrap().as_slice(),
            ["Bearer jeton-sso", "cle TUNE-TEST-0000-0000-0000"]
        );
    }

    #[tokio::test]
    async fn plateforme_sans_paquet() {
        let id = "catalogue-essai-plateforme";
        let site = site_factice(
            Catalogue::Refus(
                404,
                json!({"error": "no_package_for_target", "targets": []}),
            ),
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{corps}");
        assert_eq!(corps["error"], "no_package_for_target", "{corps}");
        assert_eq!(corps["target"], tune_plugin_native::package::host_target());
    }

    #[tokio::test]
    async fn greffon_absent_du_catalogue() {
        let id = "catalogue-essai-absent";
        let site = site_factice(
            Catalogue::Refus(404, json!({"error": "plugin_not_found"})),
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{corps}");
        assert_eq!(corps["error"], "plugin_not_in_catalog", "{corps}");
    }

    /// Réseau : un port où personne n'écoute.
    #[tokio::test]
    async fn catalogue_injoignable() {
        let id = "catalogue-essai-reseau";
        let ecoute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", ecoute.local_addr().unwrap());
        drop(ecoute);
        let (_state, app) = serveur(&base, true, false).await;
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{corps}");
        assert_eq!(corps["error"], "catalog_unreachable", "{corps}");
    }

    #[tokio::test]
    async fn ni_sso_ni_licence() {
        crate::premium_audio_plugins::tests::dossier_de_donnees_jetable();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .license
            .update_from_server(tune_core::license::Tier::Premium, None)
            .await;
        let app = crate::routes::router_with_plugins(state.clone(), vec![]);
        let (status, corps) = installer_par_la_route(&app, "catalogue-essai-anonyme").await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{corps}");
        assert_eq!(corps["error"], "not_connected", "{corps}");
    }

    async fn lire_le_catalogue(app: &axum::Router, id: &str) -> (StatusCode, Value) {
        let reponse = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/audio-plugins/{id}/catalog"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn comparaison_des_versions() {
        assert!(plus_recente("0.4.0", "0.3.1"));
        assert!(plus_recente("0.10.0", "0.9.3"));
        assert!(plus_recente("1.0", "0.99.99"));
        assert!(!plus_recente("0.3.1", "0.3.1"));
        assert!(!plus_recente("0.3.0", "0.3.1"));
        assert!(!plus_recente("0.3", "0.3.0"));
    }

    /// Décision de Bertrand (29/09) : la carte SIGNALE une nouvelle version,
    /// l'utilisateur met à jour d'un geste ; rien ne s'installe tout seul.
    #[tokio::test]
    async fn le_catalogue_signale_une_mise_a_jour_et_la_route_d_installation_la_fait() {
        let id = "catalogue-essai-maj";
        let site = site_factice(fiche_signee(id), false).await;
        let base = site.base.lock().unwrap().clone();
        let (state, app) = serveur(&base, true, false).await;

        // Pas encore installé : disponible, pas de mise à jour.
        let (status, etat) = lire_le_catalogue(&app, id).await;
        assert_eq!(status, StatusCode::OK, "{etat}");
        assert_eq!(etat["available"], true, "{etat}");
        assert_eq!(etat["installed"], false, "{etat}");
        assert_eq!(etat["latest_version"], "0.3.1", "{etat}");
        assert_eq!(etat["update_available"], false, "{etat}");
        assert_eq!(etat["target"], tune_plugin_native::package::host_target());

        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::OK, "{corps}");
        let (_, etat) = lire_le_catalogue(&app, id).await;
        assert_eq!(etat["installed_version"], "0.3.1", "{etat}");
        assert_eq!(etat["update_available"], false, "{etat}");

        // Le catalogue publie 0.4.0 : signalé, PAS installé.
        *site.version.lock().unwrap() = "0.4.0".to_string();
        let (_, etat) = lire_le_catalogue(&app, id).await;
        assert_eq!(etat["latest_version"], "0.4.0", "{etat}");
        assert_eq!(etat["update_available"], true, "{etat}");
        let settings = SettingsRepo::with_backend(state.backend.clone());
        assert_eq!(
            settings
                .get(&format!("plugin_{id}_version"))
                .unwrap()
                .as_deref(),
            Some("0.3.1"),
            "la lecture du catalogue a installé la mise à jour"
        );

        // « Mettre à jour » = la même route d'installation.
        let (status, corps) = installer_par_la_route(&app, id).await;
        assert_eq!(status, StatusCode::OK, "{corps}");
        assert_eq!(corps["version"], "0.4.0", "{corps}");
        let (_, etat) = lire_le_catalogue(&app, id).await;
        assert_eq!(etat["installed_version"], "0.4.0", "{etat}");
        assert_eq!(etat["update_available"], false, "{etat}");
    }

    /// Plateforme sans paquet : un ÉTAT (200, `available: false`), pas une
    /// panne ; la carte s'affiche grisée avec la raison.
    #[tokio::test]
    async fn le_catalogue_dit_la_plateforme_sans_paquet() {
        let id = "catalogue-essai-etat-plateforme";
        let site = site_factice(
            Catalogue::Refus(
                404,
                json!({"error": "no_package_for_target", "targets": []}),
            ),
            false,
        )
        .await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, true, false).await;
        let (status, etat) = lire_le_catalogue(&app, id).await;
        assert_eq!(status, StatusCode::OK, "{etat}");
        assert_eq!(etat["available"], false, "{etat}");
        assert_eq!(etat["reason"], "no_package_for_target", "{etat}");
        assert_eq!(etat["target"], tune_plugin_native::package::host_target());
    }

    /// Contre-épreuve : Free, la lecture du catalogue est refusée sans
    /// appeler le site.
    #[tokio::test]
    async fn le_catalogue_refuse_free_sans_appeler_le_site() {
        let id = "catalogue-essai-etat-free";
        let site = site_factice(fiche_signee(id), false).await;
        let base = site.base.lock().unwrap().clone();
        let (_state, app) = serveur(&base, false, false).await;
        let (status, etat) = lire_le_catalogue(&app, id).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{etat}");
        assert!(site.vus.lock().unwrap().is_empty(), "le site a été appelé");
    }
}
