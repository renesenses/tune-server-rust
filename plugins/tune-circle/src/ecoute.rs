//! Tune Circle, étape T4 (#5327) : écouter chez un contact, par le relais.
//!
//! | Tune (`/api/v1/ext/circle`)                              | mozaiklabs (`/api/v1/circle`)          |
//! |----------------------------------------------------------|----------------------------------------|
//! | `POST /contacts/{user_id}/listen` `{ track_id, zone_id }` | `POST /contacts/{user_id}/listen` `{ track_id }` |
//!
//! Contrat du cloud : site-mozaiklabs#237. Aucune route de présence
//! (décision 2 du 28/09) : « serveur éteint » ne s'apprend qu'en lançant
//! l'écoute, par le pont.
//! `POST …/listen`, côté AUDITEUR :
//!
//! 1. relaie la demande de billet au cloud, `track_id` tel que donné. Le cloud
//!    juge tout : contact, partage, Premium des deux côtés. Ses refus repartent
//!    tels quels (402 `premium_required`, 404, 409 `owner_unavailable`, 429) ;
//! 2. sonde `stream_url` (le pont) d'un `Range: bytes=0-0` avant de toucher à
//!    la zone : un serveur de contact éteint rend **503
//!    `circle.owner_offline`**, et la lecture en cours n'est pas coupée pour
//!    rien ;
//! 3. lance la lecture sur la zone par la route locale
//!    `POST /api/v1/zones/{zone_id}/play`, avec le corps de `corpsLecture`
//!    (`tune-web-client`, `tuneRemote.ts`) : `source: "upnp"`,
//!    `source_id` = `stream_url`, et les métadonnées de la piste. La requête
//!    part au nom de l'appelant (ses en-têtes d'authentification et de
//!    profil), exactement comme si le client web l'avait faite.
//!
//! Rend `{ "ok": true }`, ou l'erreur telle quelle.
//!
//! **Fin de partage en cours de lecture.** Le pont revérifie le billet à
//! chaque requête HTTP. Quand une zone qui joue un flux de contact tombe en
//! erreur (`zone.playback_error`), le greffon resonde le flux : si le pont
//! refuse le billet (404), il émet `circle.stream_revoked { zone_id }` pour
//! que l'écran dise « Ce partage a pris fin » plutôt qu'une erreur de
//! décodage.
//!
//! **Ce qui est gardé** : en mémoire seulement, par zone, l'adresse du flux en
//! cours — le temps de la lecture. Oubliée dès que la zone joue autre chose,
//! à la première erreur, ou au redémarrage. Rien n'est écrit en base par ce
//! module.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::event_bus::{EventBus, TuneEvent};
use tune_core::playback::PlaybackManager;

use crate::relais::{Issue, Relais};
use crate::routes::{
    CODE_CLOUD_INDISPONIBLE, CODE_REPONSE_ILLISIBLE, en_reponse, identifiant_valide, introuvable,
    refus,
};

/// Le pont dit que le serveur du contact n'est pas connecté.
pub const CODE_PROPRIETAIRE_ETEINT: &str = "circle.owner_offline";
/// `zone_id` absent ou non entier.
pub const CODE_ZONE_INVALIDE: &str = "circle.invalid_zone";
/// Événement émis quand le pont refuse le billet d'une lecture en cours.
pub const EVENEMENT_FIN_DE_PARTAGE: &str = "circle.stream_revoked";

/// Borne de la sonde du pont et de l'appel local de lecture.
const DELAI: Duration = Duration::from_secs(15);

/// En-têtes de l'appelant repris sur l'appel local de lecture : de quoi
/// passer l'authentification du serveur et viser le bon profil, rien d'autre.
const ENTETES_DE_L_APPELANT: [&str; 4] =
    ["authorization", "cookie", "x-profile-id", "accept-language"];

/// Une lecture de contact en cours, sur une zone.
#[derive(Debug, Clone)]
struct EnCours {
    stream_url: String,
}

pub struct Ecoute {
    relais: Arc<Relais>,
    /// `http://127.0.0.1:{port}` — la base de l'API de CE serveur
    /// (`PluginContext::api_base_url`).
    api_locale: String,
    bus: Option<EventBus>,
    playback: Option<Arc<PlaybackManager>>,
    en_cours: Mutex<HashMap<i64, EnCours>>,
}

impl Ecoute {
    pub fn new(
        relais: Arc<Relais>,
        api_locale: &str,
        bus: Option<EventBus>,
        playback: Option<Arc<PlaybackManager>>,
    ) -> Self {
        Self {
            relais,
            api_locale: api_locale.trim_end_matches('/').to_string(),
            bus,
            playback,
            en_cours: Mutex::new(HashMap::new()),
        }
    }

    fn noter(&self, zone_id: i64, stream_url: String) {
        if let Ok(mut m) = self.en_cours.lock() {
            m.insert(zone_id, EnCours { stream_url });
        }
    }

    fn oublier(&self, zone_id: i64) -> Option<EnCours> {
        self.en_cours.lock().ok()?.remove(&zone_id)
    }

    fn suivie(&self, zone_id: i64) -> Option<EnCours> {
        self.en_cours.lock().ok()?.get(&zone_id).cloned()
    }

    /// Les événements du bus qui concernent une lecture de contact.
    ///
    /// Rend la main tout de suite : la sonde part dans sa propre tâche, pour
    /// ne pas retenir le répartiteur d'événements des greffons.
    pub async fn sur_evenement(self: &Arc<Self>, evenement: &TuneEvent) {
        let Some(zone_id) = evenement.data.get("zone_id").and_then(Value::as_i64) else {
            return;
        };
        match evenement.event_type.as_str() {
            "zone.updated" => {
                // La zone joue autre chose : la lecture de contact est finie.
                let (Some(suivie), Some(playback)) = (self.suivie(zone_id), &self.playback) else {
                    return;
                };
                let etat = playback.get_state(zone_id).await;
                let source = etat
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.source_id.clone());
                if source.is_some_and(|s| s != suivie.stream_url) {
                    self.oublier(zone_id);
                }
            }
            "zone.playback_error" => {
                let Some(suivie) = self.oublier(zone_id) else {
                    return;
                };
                let moi = self.clone();
                tokio::spawn(async move {
                    if let Sonde::Refuse = sonder(&suivie.stream_url).await {
                        info!(zone_id, "circle_flux_revoque");
                        if let Some(bus) = &moi.bus {
                            bus.emit(EVENEMENT_FIN_DE_PARTAGE, json!({ "zone_id": zone_id }));
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

pub fn router(ecoute: Arc<Ecoute>) -> Router<()> {
    Router::new()
        .route("/contacts/{user_id}/listen", post(ecouter))
        .with_state(ecoute)
}

/// Ce que le pont dit du flux.
#[derive(Debug)]
enum Sonde {
    Ouvert,
    /// Billet refusé (404).
    Refuse,
    /// 503 `owner_offline`.
    ProprietaireEteint,
    /// Tout le reste : pont injoignable, 5xx, réponse inattendue.
    Indisponible(Option<u16>),
}

/// Un octet du flux, pour savoir ce que le pont en dit. La réponse est lâchée
/// sans être lue : rien de l'audio n'est gardé.
async fn sonder(stream_url: &str) -> Sonde {
    let envoi = tune_core::http::client::shared()
        .get(stream_url)
        .header(header::RANGE.as_str(), "bytes=0-0")
        .timeout(DELAI)
        .send()
        .await;
    let reponse = match envoi {
        Ok(r) => r,
        Err(e) => {
            // `without_url` : l'adresse porte le billet, elle ne va pas au journal.
            warn!(error = %e.without_url(), "circle_pont_injoignable");
            return Sonde::Indisponible(None);
        }
    };
    let statut = reponse.status().as_u16();
    match statut {
        200..=299 => Sonde::Ouvert,
        404 => Sonde::Refuse,
        503 => {
            let corps: Value = reponse.json().await.unwrap_or_default();
            if corps.get("code").and_then(Value::as_str) == Some("owner_offline") {
                Sonde::ProprietaireEteint
            } else {
                Sonde::Indisponible(Some(503))
            }
        }
        autre => Sonde::Indisponible(Some(autre)),
    }
}

/// Un `zone_id` entier, en nombre ou en chaîne de chiffres.
fn zone_id_de(corps: &Value) -> Option<i64> {
    match corps.get("zone_id")? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Une adresse de flux utilisable : `http` ou `https`, avec un hôte.
fn adresse_de_flux(corps: &Value) -> Option<String> {
    let brut = corps.get("stream_url")?.as_str()?.trim();
    let url = reqwest::Url::parse(brut).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host().is_some()).then(|| brut.to_string())
}

/// Le corps de `POST /zones/{id}/play`, comme `corpsLecture` (`tuneRemote.ts`).
///
/// Les champs de la projection T2 (site-mozaiklabs#233) qui ont un sens pour
/// la barre de transport et le chemin du signal ; `format` devient
/// `media_format`, pour que le MIME annoncé à la zone ne se devine pas sur
/// une URL sans extension.
pub fn corps_de_lecture(stream_url: &str, piste: &Value) -> Value {
    let mut b = json!({ "source": "upnp", "source_id": stream_url });
    for (de, vers) in [
        ("title", "title"),
        ("artist_name", "artist_name"),
        ("album_title", "album_title"),
        ("cover_url", "cover_path"),
        ("format", "media_format"),
    ] {
        if let Some(v) = piste
            .get(de)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            b[vers] = json!(v);
        }
    }
    for champ in [
        "duration_ms",
        "sample_rate",
        "bit_depth",
        "track_number",
        "disc_number",
    ] {
        if let Some(v) = piste.get(champ).and_then(Value::as_i64).filter(|n| *n > 0) {
            b[champ] = json!(v);
        }
    }
    b
}

async fn ecouter(
    State(ecoute): State<Arc<Ecoute>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    corps: Bytes,
) -> Response {
    if !identifiant_valide(&user_id) {
        return introuvable();
    }
    let demande = serde_json::from_slice::<Value>(&corps).unwrap_or(Value::Null);
    let Some(zone_id) = zone_id_de(&demande) else {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "code": CODE_ZONE_INVALIDE }),
        );
    };
    // `track_id` part tel que le client l'a donné : c'est le cloud qui le juge.
    let envoi = json!({ "track_id": demande.get("track_id").cloned().unwrap_or(Value::Null) });

    // 1. Le billet.
    let issue = ecoute
        .relais
        .appeler(
            "POST /contacts/{user_id}/listen",
            Method::POST,
            &["contacts", &user_id, "listen"],
            Some(&envoi),
        )
        .await;
    let billet = match &issue {
        Issue::Reponse { statut, corps, .. } if (200..300).contains(statut) => {
            serde_json::from_slice::<Value>(corps).ok()
        }
        _ => return en_reponse(issue),
    };
    let Some(billet) = billet else {
        return refus(
            StatusCode::BAD_GATEWAY,
            json!({ "code": CODE_REPONSE_ILLISIBLE }),
        );
    };
    let Some(stream_url) = adresse_de_flux(&billet) else {
        warn!("circle_billet_sans_adresse_de_flux");
        return refus(
            StatusCode::BAD_GATEWAY,
            json!({ "code": CODE_REPONSE_ILLISIBLE }),
        );
    };

    // 2. Le pont : le serveur du contact est-il là ?
    match sonder(&stream_url).await {
        Sonde::Ouvert => {}
        Sonde::ProprietaireEteint => {
            return refus(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "connected": true, "code": CODE_PROPRIETAIRE_ETEINT }),
            );
        }
        // Délivré puis refusé aussitôt (révoqué entre-temps) : la même 404
        // que « pas partagé ».
        Sonde::Refuse => return introuvable(),
        Sonde::Indisponible(statut_amont) => {
            return refus(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "connected": true,
                    "code": CODE_CLOUD_INDISPONIBLE,
                    "upstream_status": statut_amont,
                }),
            );
        }
    }

    // 3. La lecture sur la zone, par la route locale, au nom de l'appelant.
    let piste = billet.get("track").cloned().unwrap_or(Value::Null);
    let lecture = corps_de_lecture(&stream_url, &piste);
    let url = format!("{}/api/v1/zones/{}/play", ecoute.api_locale, zone_id);
    let mut requete = tune_core::http::client::shared()
        .post(&url)
        .json(&lecture)
        .timeout(DELAI);
    for nom in ENTETES_DE_L_APPELANT {
        if let Some(v) = headers.get(nom).and_then(|v| v.to_str().ok()) {
            requete = requete.header(nom, v);
        }
    }
    let reponse = match requete.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(zone_id, error = %e.without_url(), "circle_lecture_locale_impossible");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let statut = reponse.status();
    if statut.is_success() {
        ecoute.noter(zone_id, stream_url);
        info!(zone_id, "circle_ecoute_lancee");
        return Json(json!({ "ok": true })).into_response();
    }
    // Le refus de la route de lecture (zone inconnue, sortie absente…),
    // tel quel.
    let type_de_contenu = reponse
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let octets = reponse.bytes().await.unwrap_or_default();
    (
        StatusCode::from_u16(statut.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        [(header::CONTENT_TYPE, type_de_contenu)],
        Body::from(octets),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_corps_de_lecture_suit_corps_lecture() {
        let b = corps_de_lecture(
            "https://pont.exemple/stream/circle/b",
            &json!({
                "id": "42", "title": "So What", "artist_name": "Miles Davis",
                "album_title": "Kind of Blue", "format": "flac", "duration_ms": 562000,
                "sample_rate": 96000, "bit_depth": 24, "genre": "Jazz", "isrc": "X"
            }),
        );
        assert_eq!(
            b,
            json!({
                "source": "upnp", "source_id": "https://pont.exemple/stream/circle/b",
                "title": "So What", "artist_name": "Miles Davis", "album_title": "Kind of Blue",
                "media_format": "flac", "duration_ms": 562000, "sample_rate": 96000,
                "bit_depth": 24
            })
        );
    }

    #[test]
    fn une_adresse_de_flux_est_http_ou_https() {
        assert!(
            adresse_de_flux(&json!({"stream_url": "https://b.exemple/stream/circle/x"})).is_some()
        );
        assert!(adresse_de_flux(&json!({"stream_url": "file:///etc/passwd"})).is_none());
        assert!(adresse_de_flux(&json!({"stream_url": "pas une url"})).is_none());
        assert!(adresse_de_flux(&json!({})).is_none());
    }

    #[test]
    fn le_zone_id_est_un_entier() {
        assert_eq!(zone_id_de(&json!({"zone_id": 3})), Some(3));
        assert_eq!(zone_id_de(&json!({"zone_id": "3"})), Some(3));
        assert_eq!(zone_id_de(&json!({"zone_id": "3/../1"})), None);
        assert_eq!(zone_id_de(&json!({})), None);
    }
}
