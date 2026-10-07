//! Tune Circle, étape T4 (#5327) : écouter chez un contact, par le relais.
//!
//! | Tune (`/api/v1/ext/circle`) | mozaiklabs (`/api/v1/circle`) |
//! |---|---|
//! | `POST /contacts/{user_id}/listen` `{ track_id, zone_id }` | `POST /contacts/{user_id}/listen` `{ track_id }` |
//! | `POST /contacts/{user_id}/listen` `{ album_id, track_ids, zone_id }` | `GET /contacts/{user_id}/library/albums/{album_id}/tracks`, puis un `POST …/listen` par piste |
//!
//! Contrat du cloud : site-mozaiklabs#237. Aucune route de présence
//! (décision 2 du 28/09) : « serveur éteint » ne s'apprend qu'à l'écoute.
//!
//! ## La file, et un billet par piste
//!
//! La lecture passe par la file de la zone, comme n'importe quel album : des
//! lignes `source: "circle"`, `source_id: "{user_id}:{track_id}"`, avec leurs
//! métadonnées. **Aucune URL n'y entre** : le billet (et donc l'adresse du
//! pont) est demandé au moment où la piste va être jouée, quand
//! l'orchestrateur résout la ligne (`tune_core::source_url` ; lecture,
//! avancement, pré-armement gapless). Chaque billet a sa propre durée de vie.
//!
//! `POST …/listen`, côté AUDITEUR :
//!
//! 1. (album) lit les métadonnées de l'album chez le cloud et ne garde que les
//!    pistes demandées, dans l'ordre de `track_ids` ;
//! 2. demande le billet de la PREMIÈRE piste : les refus du cloud repartent
//!    tels quels (402 `premium_required`, 404, 409 `owner_unavailable`, 429) ;
//! 3. sonde l'adresse du pont d'un `Range: bytes=0-0` avant de toucher à la
//!    zone : serveur du contact éteint → 503 `circle.owner_offline`, et la
//!    lecture en cours n'est pas coupée pour rien ;
//! 4. pose la file et lance la première ligne. Ce premier billet sert à la
//!    première résolution : un seul billet pour cette piste.
//!
//! Pendant la lecture, à chaque piste suivante :
//! * refus du cloud pour cette piste (404, 409) ou billet refusé par le pont :
//!   la piste est sautée (`playback.track_skipped`) ;
//! * 503 `owner_offline` du pont (ou 402, session finie) : la file s'arrête,
//!   et le greffon émet `circle.owner_offline { zone_id, user_id }`.
//!
//! **Fin de partage en cours de lecture.** Quand une zone qui joue un flux de
//! contact tombe en erreur (`zone.playback_error`), le greffon resonde le
//! dernier flux fourni : si le pont refuse le billet (404), il émet
//! `circle.stream_revoked { zone_id }`.
//!
//! **Ce qui est gardé**, en mémoire seulement : par zone, la dernière adresse
//! fournie (pour la sonde ci-dessus) et le billet de la première piste le
//! temps de sa résolution. Rien n'est écrit en base par ce module hormis la
//! file elle-même, qui ne porte que des références.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::event_bus::{EventBus, TuneEvent};
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::playback::PlaybackManager;
use tune_core::source_url::{FournisseurDUrl, RefusDUrl, UrlFournie};

use crate::relais::{Issue, Relais};
use crate::routes::{
    CODE_CLOUD_INDISPONIBLE, CODE_REPONSE_ILLISIBLE, en_reponse, identifiant_valide, introuvable,
    refus,
};

/// La `source` des lignes de file d'une écoute de contact.
pub const SOURCE: &str = "circle";
/// Le pont dit que le serveur du contact n'est pas connecté.
pub const CODE_PROPRIETAIRE_ETEINT: &str = "circle.owner_offline";
/// `zone_id` absent ou non entier.
pub const CODE_ZONE_INVALIDE: &str = "circle.invalid_zone";
/// `track_ids` vide, trop long, ou pas une liste d'entiers ; `album_id` absent.
pub const CODE_PISTES_INVALIDES: &str = "circle.invalid_tracks";
/// La zone n'a pas pu lancer la lecture.
pub const CODE_LECTURE_IMPOSSIBLE: &str = "circle.playback_failed";
/// Événement émis quand le pont refuse le billet d'une lecture en cours.
pub const EVENEMENT_FIN_DE_PARTAGE: &str = "circle.stream_revoked";
/// Événement émis quand la file s'arrête parce que le serveur du contact est
/// éteint.
pub const EVENEMENT_PROPRIETAIRE_ETEINT: &str = "circle.owner_offline";

/// Borne de la sonde du pont.
const DELAI: Duration = Duration::from_secs(15);
/// Au-delà, une demande n'est pas un album.
const PISTES_MAX: usize = 500;

/// Une ligne de la file d'une écoute de contact.
#[derive(Debug, Clone, PartialEq)]
pub struct Ligne {
    /// `{user_id}:{track_id}` — jamais une URL.
    pub reference: String,
    pub titre: String,
    pub artiste: String,
    pub album: Option<String>,
    pub duree_ms: i64,
    pub numero: Option<i64>,
    pub disque: Option<i64>,
    pub media_format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
}

impl Ligne {
    /// Depuis une piste de la projection T2 (site-mozaiklabs#233).
    fn depuis_la_projection(user_id: &str, track_id: &str, piste: &Value) -> Self {
        let texte = |cle: &str| {
            piste
                .get(cle)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let entier = |cle: &str| piste.get(cle).and_then(Value::as_i64).filter(|n| *n > 0);
        Self {
            reference: format!("{user_id}:{track_id}"),
            titre: texte("title").unwrap_or_default(),
            artiste: texte("artist_name").unwrap_or_default(),
            album: texte("album_title"),
            duree_ms: entier("duration_ms").unwrap_or(0),
            numero: entier("track_number"),
            disque: entier("disc_number"),
            media_format: texte("format"),
            sample_rate: entier("sample_rate").and_then(|n| u32::try_from(n).ok()),
            bit_depth: entier("bit_depth").and_then(|n| u16::try_from(n).ok()),
        }
    }
}

/// Ce que le greffon demande à l'hôte pour jouer : poser une file sur une
/// zone et en lancer la première ligne. Un trait, pour que les routes se
/// prouvent sans orchestrateur.
#[async_trait]
pub trait HoteLecture: Send + Sync {
    async fn jouer_file(&self, zone_id: i64, lignes: Vec<Ligne>) -> Result<(), String>;
}

/// L'hôte de production : ce que fait `POST /zones/{id}/play` pour un album de
/// service, comme le greffon `cd` (`plugins/tune-cd/src/hote.rs`).
pub struct HoteOrchestrateur {
    pub backend: Arc<dyn DbBackend>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub playback: Arc<PlaybackManager>,
}

#[async_trait]
impl HoteLecture for HoteOrchestrateur {
    async fn jouer_file(&self, zone_id: i64, lignes: Vec<Ligne>) -> Result<(), String> {
        let premiere = lignes.first().cloned().ok_or("file vide")?;
        let file: Vec<_> = lignes
            .iter()
            .map(|l| {
                (
                    l.reference.clone(),
                    l.titre.clone(),
                    l.artiste.clone(),
                    l.album.clone(),
                    None,
                    l.duree_ms,
                    Some(SOURCE.to_string()),
                    l.numero,
                    l.disque,
                )
            })
            .collect();
        PlayQueueRepo::with_backend(self.backend.clone()).set_streaming_queue(zone_id, &file)?;
        let longueur = lignes.len() as i64;
        self.playback.update_queue_info(zone_id, 0, longueur).await;
        let output_device_id = ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let resultat = self
            .orchestrator
            .play(PlayRequest {
                zone_id,
                output_device_id,
                track_id: None,
                source: Some(SOURCE.into()),
                source_id: Some(premiere.reference),
                title: Some(premiere.titre),
                artist_name: Some(premiere.artiste),
                album_title: premiere.album,
                cover_url: None,
                duration_ms: Some(premiere.duree_ms),
                seek_ms: None,
                temp_file_path: None,
                sample_rate: premiere.sample_rate,
                bit_depth: premiere.bit_depth,
                media_format: premiere.media_format,
                track_number: premiere.numero.map(|n| n as u32),
                disc_number: premiere.disque.map(|n| n as u32),
                album_ref: None,
            })
            .await?;
        // Réaffirmée APRÈS play(), comme `POST /zones/{id}/play` : sur une zone
        // neuve, play() crée l'état avec une file de longueur 0.
        self.playback.update_queue_info(zone_id, 0, longueur).await;
        match resultat.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

pub struct Ecoute {
    relais: Arc<Relais>,
    bus: Option<EventBus>,
    playback: Option<Arc<PlaybackManager>>,
    hote: Option<Arc<dyn HoteLecture>>,
    /// Par zone, la dernière adresse fournie (pour `circle.stream_revoked`).
    en_cours: Mutex<HashMap<i64, String>>,
    /// Le billet de la première piste, le temps de sa première résolution.
    prets: Mutex<HashMap<(i64, String), UrlFournie>>,
}

impl Ecoute {
    pub fn new(
        relais: Arc<Relais>,
        bus: Option<EventBus>,
        playback: Option<Arc<PlaybackManager>>,
        hote: Option<Arc<dyn HoteLecture>>,
    ) -> Self {
        Self {
            relais,
            bus,
            playback,
            hote,
            en_cours: Mutex::new(HashMap::new()),
            prets: Mutex::new(HashMap::new()),
        }
    }

    fn noter(&self, zone_id: i64, url: &str) {
        if let Ok(mut m) = self.en_cours.lock() {
            m.insert(zone_id, url.to_string());
        }
    }

    fn oublier(&self, zone_id: i64) -> Option<String> {
        self.en_cours.lock().ok()?.remove(&zone_id)
    }

    fn suivie(&self, zone_id: i64) -> bool {
        self.en_cours.lock().is_ok_and(|m| m.contains_key(&zone_id))
    }

    fn emettre(&self, evenement: &str, charge: Value) {
        if let Some(bus) = &self.bus {
            bus.emit(evenement, charge);
        }
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
                let (true, Some(playback)) = (self.suivie(zone_id), &self.playback) else {
                    return;
                };
                let etat = playback.get_state(zone_id).await;
                if etat
                    .now_playing
                    .as_ref()
                    .is_some_and(|np| np.source != SOURCE)
                {
                    self.oublier(zone_id);
                }
            }
            "zone.playback_error" => {
                let Some(url) = self.oublier(zone_id) else {
                    return;
                };
                let moi = self.clone();
                tokio::spawn(async move {
                    if let Sonde::Refuse = sonder(&url).await {
                        info!(zone_id, "circle_flux_revoque");
                        moi.emettre(EVENEMENT_FIN_DE_PARTAGE, json!({ "zone_id": zone_id }));
                    }
                });
            }
            _ => {}
        }
    }

    /// Un billet pour `(user_id, track_id)`, sondé auprès du pont.
    async fn billet(&self, user_id: &str, track_id: &Value) -> Billet {
        let envoi = json!({ "track_id": track_id });
        let issue = self
            .relais
            .appeler(
                "POST /contacts/{user_id}/listen",
                Method::POST,
                &["contacts", user_id, "listen"],
                Some(&envoi),
            )
            .await;
        let corps = match &issue {
            Issue::Reponse { statut, corps, .. } if (200..300).contains(statut) => {
                serde_json::from_slice::<Value>(corps).ok()
            }
            _ => return Billet::Refus(issue),
        };
        let Some(corps) = corps else {
            return Billet::Illisible;
        };
        let Some(url) = adresse_de_flux(&corps) else {
            warn!("circle_billet_sans_adresse_de_flux");
            return Billet::Illisible;
        };
        let piste = corps.get("track").cloned().unwrap_or(Value::Null);
        match sonder(&url).await {
            Sonde::Ouvert => Billet::Pret {
                fournie: UrlFournie {
                    url,
                    media_format: piste
                        .get("format")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                },
                piste,
            },
            Sonde::ProprietaireEteint => Billet::ProprietaireEteint,
            Sonde::Refuse => Billet::PontRefuse,
            Sonde::Indisponible(statut) => Billet::PontIndisponible(statut),
        }
    }
}

/// Ce qu'a donné une demande de billet.
enum Billet {
    Pret {
        fournie: UrlFournie,
        piste: Value,
    },
    /// Le cloud n'a pas délivré de billet : sa réponse, telle quelle.
    Refus(Issue),
    Illisible,
    ProprietaireEteint,
    PontRefuse,
    PontIndisponible(Option<u16>),
}

/// Le code d'erreur d'une réponse du cloud (`{"error": …}`), ou son statut.
fn code_du_cloud(statut: u16, corps: &[u8]) -> String {
    serde_json::from_slice::<Value>(corps)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| statut.to_string())
}

/// `{user_id}:{track_id}`, deux suites de chiffres.
fn lire_la_reference(reference: &str) -> Option<(&str, &str)> {
    let (u, t) = reference.split_once(':')?;
    (que_des_chiffres(u) && que_des_chiffres(t)).then_some((u, t))
}

fn que_des_chiffres(s: &str) -> bool {
    !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit())
}

/// Un identifiant de piste en chiffres, depuis un entier ou une chaîne JSON.
fn chiffres(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        Value::String(s) if que_des_chiffres(s.trim()) => Some(s.trim().to_string()),
        _ => None,
    }
}

/// La résolution d'une ligne `circle` par l'orchestrateur : un billet,
/// maintenant.
#[async_trait]
impl FournisseurDUrl for Ecoute {
    async fn url(&self, zone_id: i64, source_id: &str) -> Result<UrlFournie, RefusDUrl> {
        let Some((user_id, track_id)) = lire_la_reference(source_id) else {
            return Err(RefusDUrl::Piste("reference".into()));
        };
        // Le billet de la première piste, obtenu par la route : servi une fois.
        let pret = self
            .prets
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&(zone_id, source_id.to_string())));
        if let Some(fournie) = pret {
            self.noter(zone_id, &fournie.url);
            return Ok(fournie);
        }
        match self.billet(user_id, &json!(track_id)).await {
            Billet::Pret { fournie, .. } => {
                self.noter(zone_id, &fournie.url);
                Ok(fournie)
            }
            Billet::Refus(Issue::Reponse { statut, corps, .. }) => match statut {
                // Le Premium de l'auditeur : aucune piste ne passera.
                402 => Err(RefusDUrl::ArretDeLaFile("premium_required".into())),
                429 => Err(RefusDUrl::Panne("429".into())),
                _ => Err(RefusDUrl::Piste(code_du_cloud(statut, &corps))),
            },
            Billet::Refus(Issue::NonConnecte) => {
                Err(RefusDUrl::ArretDeLaFile("not_connected".into()))
            }
            Billet::Refus(Issue::Indisponible { .. }) | Billet::Illisible => {
                Err(RefusDUrl::Panne("cloud_unavailable".into()))
            }
            Billet::PontIndisponible(_) => Err(RefusDUrl::Panne("bridge_unavailable".into())),
            Billet::PontRefuse => Err(RefusDUrl::Piste("revoked".into())),
            Billet::ProprietaireEteint => {
                info!(zone_id, "circle_file_arretee_proprietaire_eteint");
                self.emettre(
                    EVENEMENT_PROPRIETAIRE_ETEINT,
                    json!({ "zone_id": zone_id, "user_id": user_id }),
                );
                Err(RefusDUrl::ArretDeLaFile("owner_offline".into()))
            }
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

/// Les pistes d'un album demandées, dans l'ordre de `track_ids`, depuis la
/// liste que rend le cloud (`{ data: [...] }`). Une piste absente de l'album
/// est ignorée.
fn pistes_demandees(album: &Value, track_ids: &[String]) -> Vec<(String, Value)> {
    let data = album
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let par_id: HashMap<String, Value> = data
        .into_iter()
        .filter_map(|p| chiffres(p.get("id")?).map(|id| (id, p)))
        .collect();
    track_ids
        .iter()
        .filter_map(|id| par_id.get(id).map(|p| (id.clone(), p.clone())))
        .collect()
}

fn invalide(code: &str) -> Response {
    refus(StatusCode::UNPROCESSABLE_ENTITY, json!({ "code": code }))
}

async fn ecouter(
    State(ecoute): State<Arc<Ecoute>>,
    Path(user_id): Path<String>,
    corps: Bytes,
) -> Response {
    // La référence d'une ligne est `{user_id}:{track_id}` : un identifiant de
    // compte est un entier (site-mozaiklabs#233), tout le reste est inconnu.
    if !identifiant_valide(&user_id) || !que_des_chiffres(&user_id) {
        return introuvable();
    }
    let demande = serde_json::from_slice::<Value>(&corps).unwrap_or(Value::Null);
    let Some(zone_id) = zone_id_de(&demande) else {
        return invalide(CODE_ZONE_INVALIDE);
    };
    let Some(hote) = ecoute.hote.clone() else {
        warn!("circle_ecoute_sans_hote_de_lecture");
        return refus(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "code": CODE_LECTURE_IMPOSSIBLE }),
        );
    };

    // L'album : les pistes demandées, dans l'ordre demandé, avec leurs
    // métadonnées lues chez le cloud.
    let album = match demande.get("track_ids") {
        None => None,
        Some(liste) => {
            let Some(ids) = liste
                .as_array()
                .filter(|l| !l.is_empty() && l.len() <= PISTES_MAX)
                .and_then(|l| l.iter().map(chiffres).collect::<Option<Vec<_>>>())
            else {
                return invalide(CODE_PISTES_INVALIDES);
            };
            let Some(album_id) = demande.get("album_id").and_then(chiffres) else {
                return invalide(CODE_PISTES_INVALIDES);
            };
            let issue = ecoute
                .relais
                .appeler(
                    "GET /contacts/{user_id}/library/albums/{album_id}/tracks",
                    Method::GET,
                    &[
                        "contacts", &user_id, "library", "albums", &album_id, "tracks",
                    ],
                    None,
                )
                .await;
            let liste = match &issue {
                Issue::Reponse { statut, corps, .. } if (200..300).contains(statut) => {
                    serde_json::from_slice::<Value>(corps).ok()
                }
                _ => return en_reponse(issue),
            };
            let Some(liste) = liste else {
                return refus(
                    StatusCode::BAD_GATEWAY,
                    json!({ "code": CODE_REPONSE_ILLISIBLE }),
                );
            };
            let pistes = pistes_demandees(&liste, &ids);
            if pistes.is_empty() {
                return introuvable();
            }
            Some(pistes)
        }
    };

    // Le billet de la première piste.
    let premier_id = match &album {
        Some(pistes) => json!(pistes[0].0),
        // `track_id` part tel que le client l'a donné : c'est le cloud qui le
        // juge (comportement de la première version, inchangé).
        None => demande.get("track_id").cloned().unwrap_or(Value::Null),
    };
    let (fournie, piste) = match ecoute.billet(&user_id, &premier_id).await {
        Billet::Pret { fournie, piste } => (fournie, piste),
        Billet::Refus(issue) => return en_reponse(issue),
        Billet::Illisible => {
            return refus(
                StatusCode::BAD_GATEWAY,
                json!({ "code": CODE_REPONSE_ILLISIBLE }),
            );
        }
        Billet::ProprietaireEteint => {
            return refus(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "connected": true, "code": CODE_PROPRIETAIRE_ETEINT }),
            );
        }
        // Délivré puis refusé aussitôt (révoqué entre-temps) : la même 404
        // que « pas partagé ».
        Billet::PontRefuse => return introuvable(),
        Billet::PontIndisponible(statut_amont) => {
            return refus(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "connected": true,
                    "code": CODE_CLOUD_INDISPONIBLE,
                    "upstream_status": statut_amont,
                }),
            );
        }
    };

    let lignes: Vec<Ligne> = match album {
        Some(pistes) => pistes
            .iter()
            .map(|(id, p)| Ligne::depuis_la_projection(&user_id, id, p))
            .collect(),
        None => {
            // L'identifiant canonique est celui que rend le cloud.
            let Some(id) = piste
                .get("id")
                .and_then(chiffres)
                .or_else(|| chiffres(&premier_id))
            else {
                return refus(
                    StatusCode::BAD_GATEWAY,
                    json!({ "code": CODE_REPONSE_ILLISIBLE }),
                );
            };
            vec![Ligne::depuis_la_projection(&user_id, &id, &piste)]
        }
    };
    let nombre = lignes.len();
    let est_un_album = demande.get("track_ids").is_some();
    let cle = (zone_id, lignes[0].reference.clone());
    if let Ok(mut m) = ecoute.prets.lock() {
        m.insert(cle.clone(), fournie);
    }
    match hote.jouer_file(zone_id, lignes).await {
        Ok(()) => {
            info!(zone_id, pistes = nombre, "circle_ecoute_lancee");
            if est_un_album {
                Json(json!({ "ok": true, "queued": nombre })).into_response()
            } else {
                Json(json!({ "ok": true })).into_response()
            }
        }
        Err(e) => {
            if let Ok(mut m) = ecoute.prets.lock() {
                m.remove(&cle);
            }
            warn!(zone_id, error = %e, "circle_lecture_impossible");
            refus(
                StatusCode::CONFLICT,
                json!({ "code": CODE_LECTURE_IMPOSSIBLE, "error": e }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_ligne_suit_la_projection_t2() {
        let l = Ligne::depuis_la_projection(
            "7",
            "42",
            &json!({
                "id": 42, "title": "So What", "artist_name": "Miles Davis",
                "album_title": "Kind of Blue", "format": "flac", "duration_ms": 562000,
                "sample_rate": 96000, "bit_depth": 24, "track_number": 1, "disc_number": 1,
                "genre": "Jazz", "isrc": "X"
            }),
        );
        assert_eq!(l.reference, "7:42");
        assert_eq!(l.titre, "So What");
        assert_eq!(l.media_format.as_deref(), Some("flac"));
        assert_eq!((l.sample_rate, l.bit_depth), (Some(96000), Some(24)));
        assert_eq!((l.numero, l.disque, l.duree_ms), (Some(1), Some(1), 562000));
    }

    #[test]
    fn une_reference_est_deux_suites_de_chiffres() {
        assert_eq!(lire_la_reference("7:42"), Some(("7", "42")));
        for mauvaise in ["7", "7:", ":42", "7:4/2", "a:1", "7:42:1", "7:../1", "-7:1"] {
            assert_eq!(lire_la_reference(mauvaise), None, "{mauvaise}");
        }
    }

    #[test]
    fn les_pistes_demandees_suivent_l_ordre_demande() {
        let album = json!({ "data": [
            { "id": 1, "title": "Un" }, { "id": 2, "title": "Deux" }, { "id": "3", "title": "Trois" }
        ]});
        let ids = ["3", "1", "9"].map(String::from);
        let p = pistes_demandees(&album, &ids);
        assert_eq!(
            p.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["3", "1"]
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
