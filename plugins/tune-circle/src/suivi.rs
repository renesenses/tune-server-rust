//! #6688 : le SUIVI d'une playlist locale partagée avec un cercle.
//!
//! « Partager avec un cercle » copiait une seule fois la playlist locale dans
//! une playlist de cercle neuve (`bulk-items`) : rien de ce qui changeait
//! ensuite dans l'originale n'atteignait le cercle, et rien ne le disait
//! (Bertrand, 21/10). Décision de Bertrand : suivi automatique.
//!
//! * **Le lien.** `PUT /playlists/{id}/follow { playlist_id }` relie la
//!   playlist de cercle `{id}` à la playlist locale `playlist_id`. Il vit dans
//!   les réglages ([`PREFIXE_SUIVI`]), comme [`crate::playlists::PREFIXE_COPIE_EN_COURS`] :
//!   aucune migration, et rien du cercle n'y est gardé — deux identifiants,
//!   l'état du dernier report et un éventuel avertissement.
//! * **Le report.** L'originale est relue ; si son contenu (références et
//!   nom, résumés par une empreinte) a changé depuis le dernier report, la
//!   playlist de cercle est relue au cloud et rendue identique à l'originale
//!   par le relais existant : retraits (`DELETE …/items/{item_id}?version=`),
//!   ajouts en fin (`POST …/items`, par lots de [`AJOUTS_MAX`]), ordre
//!   (`PUT …/order`), nom (`PATCH`). Chaque écriture porte la version rendue
//!   par la précédente ; un 409 relit tout et rejoue le report UNE fois.
//! * **L'originale l'emporte.** Si la playlist de cercle a changé depuis le
//!   dernier report (un membre a ajouté, retiré, déplacé ou renommé), ses
//!   changements sont remplacés. L'écart est journalisé
//!   (`circle_suivi_ecart`), gardé dans le lien ([`CODE_ECART`]) — rendu par
//!   `GET /playlists/{id}`, `GET …/follow` et le report lui-même — et publié
//!   sur le bus (`circle.playlist.follow_overwritten`).
//! * **Cloud injoignable.** Le lien passe « en attente » (`pending`) ; la
//!   boucle de fond rejoue le report au retour. Le report étant un état et non
//!   une suite de gestes, rejouer l'originale du moment ne perd rien.
//! * **« Ne plus suivre »** : `DELETE /playlists/{id}/follow` ARRÊTE le lien
//!   (`stopped_at`) ; plus aucun report ne part. Le lien arrêté est gardé :
//!   le badge « Partagée avec un cercle » de la playlist locale dit alors
//!   « suivi arrêté ». Un nouveau `PUT …/follow` le relance, en gardant
//!   l'empreinte du dernier report : ce que le cercle a changé pendant
//!   l'arrêt est remplacé AVEC l'avertissement d'écart. Un premier lien sur
//!   une playlist de cercle déjà garnie et différente l'avertit aussi.
//! * **Le badge** : `GET /api/v1/playlists` et `GET /api/v1/playlists/{id}`
//!   (routes du serveur) lisent ces liens et exposent `cercles`.
//! * Une playlist locale qui n'est suivie par aucun cercle ne provoque AUCUN
//!   appel au cloud : la boucle ne parcourt que les liens, et un lien dont
//!   l'empreinte n'a pas bougé n'appelle pas le cloud non plus.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tracing::{info, warn};
use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;

use crate::playlists::{AJOUTS_MAX, MORCEAUX_MAX, profil_actif_de};
use crate::references::{self, CLES_DE_REFERENCE};
use crate::relais::{Issue, Relais};
use crate::routes::{en_reponse, identifiant_valide, introuvable, refus};

/// Préfixe du réglage d'un lien : `circle_suivi:{id de la playlist de cercle}`
/// → le lien, en JSON.
pub const PREFIXE_SUIVI: &str = "circle_suivi:";

/// L'avertissement d'un report qui a remplacé des changements faits DANS le
/// cercle depuis le report précédent.
pub const CODE_ECART: &str = "circle.follow_overwritten";
/// La playlist locale suivie n'existe plus : plus rien n'est reporté.
pub const CODE_SOURCE_DISPARUE: &str = "circle.follow_source_missing";
/// `PUT …/follow` sans `playlist_id` entier, ou une playlist locale inconnue.
pub const CODE_SOURCE_INCONNUE: &str = "circle.unknown_local_playlist";
/// Aucun lien pour cette playlist de cercle.
pub const CODE_NON_SUIVIE: &str = "circle.not_following";
/// L'évènement publié quand un report remplace les changements du cercle.
pub const EVENEMENT_ECART: &str = "circle.playlist.follow_overwritten";

/// Période de la boucle de fond : relire les liens, reporter ce qui a changé,
/// rejouer ce qui attend.
pub const PERIODE: Duration = Duration::from_secs(20);

/// Le lien d'une playlist de cercle vers sa playlist locale d'origine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lien {
    pub circle_playlist_id: String,
    pub local_playlist_id: i64,
    /// Le cercle de la playlist partagée, pour le badge « Partagée avec un
    /// cercle » des playlists locales : son identifiant et son nom, donnés
    /// au partage ou lus au cloud.
    pub circle_id: Value,
    pub circle_name: Option<String>,
    /// « Ne plus suivre » : le lien est GARDÉ, arrêté, pour que le badge dise
    /// « suivi arrêté » ; plus aucun report ne part.
    pub stopped_at: Option<String>,
    /// Le nom de l'originale au dernier report : « Suit la playlist « … » ».
    pub local_name: String,
    pub followed_at: String,
    /// Date du dernier report réussi (RFC 3339).
    pub last_sync: Option<String>,
    /// Version de la playlist de cercle après le dernier report.
    pub synced_version: Option<i64>,
    /// Empreinte de l'originale au dernier report.
    pub source_digest: Option<String>,
    /// Empreinte de la playlist de cercle (nom et références) après le
    /// dernier report : ce qui permet de voir que le cercle l'a changée.
    pub circle_digest: Option<String>,
    /// Un report attend le retour du cloud.
    pub pending: bool,
    pub pending_since: Option<String>,
    /// Le dernier refus (`{ status, code }`), effacé par un report réussi.
    pub last_error: Option<Value>,
    /// Le dernier avertissement ([`CODE_ECART`], [`CODE_SOURCE_DISPARUE`]).
    pub warning: Option<Value>,
}

impl Lien {
    fn cle(&self) -> String {
        format!("{PREFIXE_SUIVI}{}", self.circle_playlist_id)
    }

    fn en_json(&self) -> Value {
        json!({
            "circle_playlist_id": self.circle_playlist_id,
            "local_playlist_id": self.local_playlist_id,
            "circle_id": self.circle_id,
            "circle_name": self.circle_name,
            "stopped_at": self.stopped_at,
            "local_name": self.local_name,
            "followed_at": self.followed_at,
            "last_sync": self.last_sync,
            "synced_version": self.synced_version,
            "source_digest": self.source_digest,
            "circle_digest": self.circle_digest,
            "pending": self.pending,
            "pending_since": self.pending_since,
            "last_error": self.last_error,
            "warning": self.warning,
        })
    }

    fn depuis_json(v: &Value) -> Option<Self> {
        let texte = |k: &str| v[k].as_str().map(str::to_string);
        Some(Self {
            circle_playlist_id: texte("circle_playlist_id")?,
            local_playlist_id: v["local_playlist_id"].as_i64()?,
            circle_id: v["circle_id"].clone(),
            circle_name: texte("circle_name"),
            stopped_at: texte("stopped_at"),
            local_name: texte("local_name").unwrap_or_default(),
            followed_at: texte("followed_at").unwrap_or_default(),
            last_sync: texte("last_sync"),
            synced_version: v["synced_version"].as_i64(),
            source_digest: texte("source_digest"),
            circle_digest: texte("circle_digest"),
            pending: v["pending"].as_bool().unwrap_or(false),
            pending_since: texte("pending_since"),
            last_error: Some(v["last_error"].clone()).filter(|e| !e.is_null()),
            warning: Some(v["warning"].clone()).filter(|e| !e.is_null()),
        })
    }

    /// Ce que l'écran affiche : « Suit la playlist « … » », la date du
    /// dernier report, l'attente, l'avertissement.
    pub fn pour_le_client(&self) -> Value {
        json!({
            "following": self.stopped_at.is_none(),
            "stopped_at": self.stopped_at,
            "circle_id": self.circle_id,
            "circle_name": self.circle_name,
            "local_playlist_id": self.local_playlist_id,
            "local_name": self.local_name,
            "followed_at": self.followed_at,
            "last_sync": self.last_sync,
            "pending": self.pending,
            "pending_since": self.pending_since,
            "last_error": self.last_error,
            "warning": self.warning,
        })
    }
}

fn maintenant() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Le lien de la playlist de cercle `id`, s'il existe.
pub fn lien(reglages: &SettingsRepo, id: &str) -> Option<Lien> {
    let brut = reglages
        .get(&format!("{PREFIXE_SUIVI}{id}"))
        .ok()
        .flatten()?;
    Lien::depuis_json(&serde_json::from_str(&brut).ok()?)
}

/// Tous les liens de ce serveur.
pub fn liens(reglages: &SettingsRepo) -> Vec<Lien> {
    reglages
        .all()
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| k.starts_with(PREFIXE_SUIVI))
        .filter_map(|(_, v)| Lien::depuis_json(&serde_json::from_str(&v).ok()?))
        .collect()
}

fn ecrire(reglages: &SettingsRepo, l: &Lien) {
    if let Err(e) = reglages.set(&l.cle(), &l.en_json().to_string()) {
        warn!(error = %e, "circle_suivi_lien_non_ecrit");
    }
}

/// Une référence sous une forme comparable : ses clés du contrat, non
/// nulles, dans l'ordre du contrat. Les métadonnées du cloud (`item_id`,
/// `added_by`, `added_at`…) n'en font pas partie.
pub fn cle_de_reference(r: &Value) -> String {
    let mut sortie = serde_json::Map::new();
    for k in CLES_DE_REFERENCE {
        if let Some(v) = r.get(k).filter(|v| !v.is_null()) {
            sortie.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(sortie).to_string()
}

fn empreinte(nom: &str, cles: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(nom.as_bytes());
    for c in cles {
        h.update([0u8]);
        h.update(c.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// L'originale, telle qu'elle doit être reportée.
struct Originale {
    nom: String,
    references: Vec<Value>,
    /// Lignes qui ne donnent pas de référence (ligne introuvable, titre d'un
    /// service hors contrat) : comptées, jamais envoyées.
    non_referencables: usize,
    empreinte: String,
}

/// Une playlist de cercle relue au cloud.
struct DuCercle {
    playlist: Value,
    version: Value,
}

impl DuCercle {
    fn items(&self) -> Vec<Value> {
        self.playlist["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn nom(&self) -> String {
        self.playlist["name"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn empreinte(&self) -> String {
        let cles: Vec<String> = self.items().iter().map(cle_de_reference).collect();
        empreinte(&self.nom(), &cles)
    }
}

/// Le corps JSON d'une réponse 2xx du cloud qui porte une playlist.
fn playlist_de(issue: &Issue) -> Option<DuCercle> {
    match issue {
        Issue::Reponse { statut, corps, .. } if (200..300).contains(statut) => {
            let p = serde_json::from_slice::<Value>(corps).ok()?;
            p["items"].is_array().then(|| DuCercle {
                version: p["version"].clone(),
                playlist: p,
            })
        }
        _ => None,
    }
}

/// `{ status, code }` d'un refus.
fn motif(issue: &Issue) -> Value {
    match issue {
        Issue::NonConnecte => json!({ "status": 412, "code": crate::routes::CODE_NON_CONNECTE }),
        Issue::Indisponible { statut_amont } => json!({
            "status": 503,
            "code": crate::routes::CODE_CLOUD_INDISPONIBLE,
            "upstream_status": statut_amont,
        }),
        Issue::Reponse { statut, corps, .. } => {
            let c = serde_json::from_slice::<Value>(corps).unwrap_or(Value::Null);
            let code = c["code"].as_str().or_else(|| c["error"].as_str());
            json!({ "status": statut, "code": code })
        }
    }
}

/// Un refus qui peut passer tout seul : le cloud n'a pas répondu, la session
/// SSO manque, ou le débit est dépassé. Le report attend et sera rejoué.
fn passager(issue: &Issue) -> bool {
    match issue {
        Issue::NonConnecte | Issue::Indisponible { .. } => true,
        Issue::Reponse { statut, .. } => *statut == 429,
    }
}

fn conflit(issue: &Issue) -> bool {
    matches!(issue, Issue::Reponse { statut: 409, .. })
}

/// Ce qu'a donné un report.
#[derive(Debug, Clone, PartialEq)]
pub enum Bilan {
    /// Rien n'a changé dans l'originale : aucun appel au cloud.
    Inchange,
    /// La playlist de cercle est identique à l'originale.
    Reporte {
        retires: usize,
        ajoutes: usize,
        reordonne: bool,
        renomme: bool,
        /// L'avertissement de cet appel, s'il a remplacé des changements du
        /// cercle.
        ecart: Option<Value>,
    },
    /// Le cloud n'a pas pu être joint : le report attend son retour.
    EnAttente(Value),
    /// Le cloud a refusé (et ce n'est pas passager) : le refus est gardé
    /// dans le lien, le report n'est pas rejoué tant que l'originale ne
    /// change pas.
    Refuse(Value),
    /// L'originale n'existe plus.
    SourceDisparue,
}

impl Bilan {
    pub fn en_json(&self) -> Value {
        match self {
            Self::Inchange => json!({ "status": "unchanged" }),
            Self::Reporte {
                retires,
                ajoutes,
                reordonne,
                renomme,
                ecart,
            } => json!({
                "status": "synced",
                "removed": retires,
                "added": ajoutes,
                "reordered": reordonne,
                "renamed": renomme,
                "warning": ecart,
            }),
            Self::EnAttente(m) => json!({ "status": "pending", "error": m }),
            Self::Refuse(m) => json!({ "status": "refused", "error": m }),
            Self::SourceDisparue => {
                json!({ "status": "source_missing", "code": CODE_SOURCE_DISPARUE })
            }
        }
    }
}

/// Issue interne d'une tentative de report.
enum Tentative {
    Fait(Bilan, Option<DuCercle>),
    Conflit,
    Echec(Issue),
}

pub struct Suivi {
    relais: Arc<Relais>,
    bus: Option<EventBus>,
    reveil: Notify,
    /// Un seul report à la fois : la boucle et une route ne doivent pas
    /// écrire la même playlist de cercle en même temps.
    verrou: tokio::sync::Mutex<()>,
}

impl Suivi {
    pub fn new(relais: Arc<Relais>, bus: Option<EventBus>) -> Self {
        Self {
            relais,
            bus,
            reveil: Notify::new(),
            verrou: tokio::sync::Mutex::new(()),
        }
    }

    fn reglages(&self) -> SettingsRepo {
        self.relais.reglages()
    }

    /// Réveille la boucle de fond. Ne bloque jamais.
    pub fn reveiller(&self) {
        self.reveil.notify_one();
    }

    /// Relie la playlist de cercle `id` à la playlist locale `local`, puis
    /// fait le premier report. Un lien existant est remplacé.
    pub async fn suivre(
        &self,
        id: &str,
        local: i64,
        nom: &str,
        cercle: Value,
        nom_du_cercle: Option<String>,
    ) -> (Lien, Bilan) {
        let nom_du_cercle = match nom_du_cercle {
            Some(n) => Some(n),
            None => self.nom_du_cercle(&cercle).await,
        };
        // La reprise d'un lien arrêté vers la même originale garde l'état du
        // dernier report : ce que les membres ont changé dans la copie
        // pendant l'arrêt reste un écart, signalé au premier report.
        let avant = lien(&self.reglages(), id).filter(|a| a.local_playlist_id == local);
        let l = Lien {
            circle_playlist_id: id.to_string(),
            local_playlist_id: local,
            circle_id: cercle,
            circle_name: nom_du_cercle,
            local_name: nom.to_string(),
            followed_at: maintenant(),
            synced_version: avant.as_ref().and_then(|a| a.synced_version),
            circle_digest: avant.and_then(|a| a.circle_digest),
            ..Lien::default()
        };
        ecrire(&self.reglages(), &l);
        info!(local_playlist = local, "circle_suivi_lien_cree");
        let bilan = self.reporter(id).await;
        let l = lien(&self.reglages(), id).unwrap_or(l);
        (l, bilan)
    }

    /// Le nom du cercle `id`, lu dans `GET /` du cloud (`circles`).
    async fn nom_du_cercle(&self, id: &Value) -> Option<String> {
        if id.is_null() {
            return None;
        }
        let issue = self.relais.appeler("GET /", Method::GET, &[], None).await;
        let Issue::Reponse {
            statut: 200, corps, ..
        } = issue
        else {
            return None;
        };
        let c = serde_json::from_slice::<Value>(&corps).ok()?;
        c["circles"]
            .as_array()?
            .iter()
            .find(|x| cle_d_item(&x["id"]) == cle_d_item(id))
            .and_then(|x| x["name"].as_str().map(str::to_string))
    }

    /// « Ne plus suivre » : le lien est arrêté (gardé pour le badge), plus
    /// aucun report ne part. `false` s'il n'y avait pas de lien actif.
    pub fn ne_plus_suivre(&self, id: &str) -> bool {
        let reglages = self.reglages();
        let Some(mut l) = lien(&reglages, id).filter(|l| l.stopped_at.is_none()) else {
            return false;
        };
        l.stopped_at = Some(maintenant());
        l.pending = false;
        l.pending_since = None;
        ecrire(&reglages, &l);
        info!("circle_suivi_lien_coupe");
        true
    }

    /// Un tour : chaque lien dont l'originale a changé, ou qui attend, est
    /// reporté. Rend les bilans, par playlist de cercle.
    pub async fn tour(&self) -> Vec<(String, Bilan)> {
        let mut bilans = Vec::new();
        for l in liens(&self.reglages()) {
            if l.stopped_at.is_some() {
                continue;
            }
            let b = self.reporter(&l.circle_playlist_id).await;
            bilans.push((l.circle_playlist_id, b));
        }
        bilans
    }

    /// Reporte l'originale du lien `id` dans le cercle, si elle a changé ou si
    /// un report attend.
    pub async fn reporter(&self, id: &str) -> Bilan {
        self.reporter_avec(id, false).await
    }

    /// [`Self::reporter`] ; `force` relit le cercle même si l'originale n'a
    /// pas changé (« Synchroniser maintenant »).
    pub async fn reporter_avec(&self, id: &str, force: bool) -> Bilan {
        let _garde = self.verrou.lock().await;
        let reglages = self.reglages();
        let Some(mut l) = lien(&reglages, id).filter(|l| l.stopped_at.is_none()) else {
            return Bilan::Refuse(json!({ "status": 404, "code": CODE_NON_SUIVIE }));
        };
        let Some(originale) = self.lire_l_originale(l.local_playlist_id) else {
            if l.warning.as_ref().map(|w| &w["code"]) != Some(&json!(CODE_SOURCE_DISPARUE)) {
                warn!(
                    local_playlist = l.local_playlist_id,
                    "circle_suivi_source_disparue"
                );
                l.warning = Some(json!({ "code": CODE_SOURCE_DISPARUE, "at": maintenant() }));
                l.pending = false;
                ecrire(&reglages, &l);
            }
            return Bilan::SourceDisparue;
        };
        if !force && !l.pending && l.source_digest.as_deref() == Some(&originale.empreinte) {
            return Bilan::Inchange;
        }
        // Un conflit de version (un membre écrit au même moment) : tout est
        // relu, et le report rejoué UNE fois.
        let mut tentative = self.tenter(&l, &originale).await;
        if matches!(tentative, Tentative::Conflit) {
            info!("circle_suivi_conflit_rejoue");
            tentative = self.tenter(&l, &originale).await;
        }
        l.local_name = originale.nom.clone();
        let bilan = match tentative {
            Tentative::Fait(bilan, cercle) => {
                if let Some(c) = &cercle {
                    l.synced_version = c.version.as_i64();
                    l.circle_digest = Some(c.empreinte());
                }
                l.source_digest = Some(originale.empreinte.clone());
                l.last_sync = Some(maintenant());
                l.pending = false;
                l.pending_since = None;
                l.last_error = None;
                if let Bilan::Reporte { ecart: Some(e), .. } = &bilan {
                    l.warning = Some(e.clone());
                }
                bilan
            }
            Tentative::Conflit => {
                // Deux conflits de suite : on réessaiera au prochain tour.
                let m = json!({ "status": 409, "code": "version_conflict" });
                self.mettre_en_attente(&mut l, m.clone());
                Bilan::EnAttente(m)
            }
            Tentative::Echec(issue) if passager(&issue) => {
                let m = motif(&issue);
                self.mettre_en_attente(&mut l, m.clone());
                Bilan::EnAttente(m)
            }
            Tentative::Echec(issue) => {
                let m = motif(&issue);
                warn!(statut = %m["status"], "circle_suivi_refuse");
                // Pas rejoué en boucle : l'empreinte est notée, le prochain
                // changement de l'originale retentera.
                l.source_digest = Some(originale.empreinte.clone());
                l.pending = false;
                l.pending_since = None;
                l.last_error = Some(m.clone());
                Bilan::Refuse(m)
            }
        };
        ecrire(&reglages, &l);
        bilan
    }

    fn mettre_en_attente(&self, l: &mut Lien, m: Value) {
        if !l.pending {
            info!(
                local_playlist = l.local_playlist_id,
                "circle_suivi_en_attente"
            );
            l.pending_since = Some(maintenant());
        }
        l.pending = true;
        l.last_error = Some(m);
    }

    fn lire_l_originale(&self, local: i64) -> Option<Originale> {
        let backend = self.relais.backend();
        let repo = PlaylistRepo::with_backend(backend.clone());
        let p = repo.get(local).ok().flatten()?;
        let lignes = repo.get_entries(local).ok()?;
        let mut refs = Vec::new();
        let mut non_referencables = 0;
        for e in &lignes {
            let r = match &e.content {
                EntryContent::Local(tid) => references::depuis_la_bibliotheque(backend, *tid)
                    .ok()
                    .flatten(),
                EntryContent::Service(s) => references::depuis_une_description(
                    &s.source.to_ascii_lowercase(),
                    &json!({
                        "source_id": s.source_id,
                        "title": s.title,
                        "artist_name": s.artist,
                        "album_title": s.album,
                        "duration_ms": s.duration_ms,
                    }),
                ),
                EntryContent::Missing(_) => None,
            };
            match r {
                Some(r) => refs.push(r),
                None => non_referencables += 1,
            }
        }
        refs.truncate(MORCEAUX_MAX);
        let cles: Vec<String> = refs.iter().map(cle_de_reference).collect();
        Some(Originale {
            empreinte: empreinte(&p.name, &cles),
            nom: p.name,
            references: refs,
            non_referencables,
        })
    }

    async fn lire_le_cercle(&self, id: &str) -> Result<DuCercle, Issue> {
        let issue = self
            .relais
            .appeler("GET /playlists/{id}", Method::GET, &["playlists", id], None)
            .await;
        playlist_de(&issue).ok_or(issue)
    }

    /// Une écriture : la playlist rendue, ou ce qui l'a empêchée.
    async fn ecrire_au_cercle(
        &self,
        route: &'static str,
        methode: Method,
        segments: &[&str],
        requete: Option<&str>,
        corps: Option<&Value>,
    ) -> Result<DuCercle, Tentative> {
        let issue = self
            .relais
            .appeler_avec(route, methode, segments, requete, corps, None)
            .await;
        if let Some(c) = playlist_de(&issue) {
            return Ok(c);
        }
        Err(if conflit(&issue) {
            Tentative::Conflit
        } else {
            Tentative::Echec(issue)
        })
    }

    /// Une tentative complète : relire le cercle, constater l'écart, puis
    /// retirer, ajouter, ordonner et renommer.
    async fn tenter(&self, l: &Lien, originale: &Originale) -> Tentative {
        let id = l.circle_playlist_id.as_str();
        let mut cercle = match self.lire_le_cercle(id).await {
            Ok(c) => c,
            Err(issue) => return Tentative::Echec(issue),
        };

        // L'écart : le cercle a changé depuis le dernier report. Sans report
        // connu (premier `PUT …/follow` sur une playlist de cercle existante),
        // tout morceau qu'elle porte déjà est à elle : le remplacer est un écart.
        let change_dans_le_cercle = match &l.circle_digest {
            Some(d) => *d != cercle.empreinte(),
            None => !cercle.items().is_empty(),
        };

        let voulues: Vec<String> = originale.references.iter().map(cle_de_reference).collect();
        // Appariement dans l'ordre : chaque référence voulue prend le premier
        // morceau du cercle qui la porte et n'est pas encore pris.
        let items = cercle.items();
        let mut pris = vec![false; items.len()];
        let cles_du_cercle: Vec<String> = items.iter().map(cle_de_reference).collect();
        let mut manquantes = Vec::new();
        for (k, v) in voulues.iter().enumerate() {
            match (0..items.len()).find(|&i| !pris[i] && &cles_du_cercle[i] == v) {
                Some(i) => pris[i] = true,
                None => manquantes.push(k),
            }
        }
        let a_retirer: Vec<Value> = items
            .iter()
            .zip(&pris)
            .filter(|(_, p)| !**p)
            .map(|(i, _)| i["item_id"].clone())
            .collect();

        // Ce que le cercle porte déjà ce que l'originale veut : rien à
        // remplacer, donc pas d'écart, même si un membre a écrit entre-temps.
        let deja_conforme = cercle.empreinte() == empreinte(&originale.nom, &voulues);
        let ecart = (change_dans_le_cercle && !deja_conforme).then(|| {
            json!({
                "code": CODE_ECART,
                "at": maintenant(),
                "expected_version": l.synced_version,
                "found_version": cercle.version,
                "discarded_items": a_retirer.len(),
                "restored_items": manquantes.len(),
            })
        });
        if let Some(e) = &ecart {
            warn!(
                local_playlist = l.local_playlist_id,
                retires = a_retirer.len(),
                restaures = manquantes.len(),
                "circle_suivi_ecart"
            );
            if let Some(bus) = &self.bus {
                bus.emit(
                    EVENEMENT_ECART,
                    json!({
                        "circle_playlist_id": id,
                        "local_playlist_id": l.local_playlist_id,
                        "warning": e,
                    }),
                );
            }
        }

        // 1. Retraits.
        for item_id in &a_retirer {
            let Some(item) = cle_d_item(item_id) else {
                continue;
            };
            let requete = format!("version={}", version_en_texte(&cercle.version));
            match self
                .ecrire_au_cercle(
                    "DELETE /playlists/{id}/items/{item_id}",
                    Method::DELETE,
                    &["playlists", id, "items", &item],
                    Some(&requete),
                    None,
                )
                .await
            {
                Ok(c) => cercle = c,
                Err(t) => return t,
            }
        }

        // 2. Ajouts, en fin, dans l'ordre de l'originale.
        let a_ajouter: Vec<Value> = manquantes
            .iter()
            .map(|&k| originale.references[k].clone())
            .collect();
        for lot in a_ajouter.chunks(AJOUTS_MAX) {
            let envoi = json!({ "items": lot, "version": cercle.version });
            match self
                .ecrire_au_cercle(
                    "POST /playlists/{id}/items",
                    Method::POST,
                    &["playlists", id, "items"],
                    None,
                    Some(&envoi),
                )
                .await
            {
                Ok(c) => cercle = c,
                Err(t) => return t,
            }
        }

        // 3. L'ordre de l'originale.
        let items = cercle.items();
        let cles: Vec<String> = items.iter().map(cle_de_reference).collect();
        let mut pris = vec![false; items.len()];
        let mut ordre = Vec::with_capacity(voulues.len());
        for v in &voulues {
            if let Some(i) = (0..items.len()).find(|&i| !pris[i] && &cles[i] == v) {
                pris[i] = true;
                ordre.push(items[i]["item_id"].clone());
            }
        }
        let actuel: Vec<Value> = items.iter().map(|i| i["item_id"].clone()).collect();
        let reordonne = ordre.len() == actuel.len() && ordre != actuel;
        if reordonne {
            let envoi = json!({ "item_ids": ordre, "version": cercle.version });
            match self
                .ecrire_au_cercle(
                    "PUT /playlists/{id}/order",
                    Method::PUT,
                    &["playlists", id, "order"],
                    None,
                    Some(&envoi),
                )
                .await
            {
                Ok(c) => cercle = c,
                Err(t) => return t,
            }
        }

        // 4. Le nom de l'originale.
        let renomme = cercle.nom() != originale.nom;
        if renomme {
            let envoi = json!({ "name": originale.nom, "version": cercle.version });
            match self
                .ecrire_au_cercle(
                    "PATCH /playlists/{id}",
                    Method::PATCH,
                    &["playlists", id],
                    None,
                    Some(&envoi),
                )
                .await
            {
                Ok(c) => cercle = c,
                Err(t) => return t,
            }
        }

        if originale.non_referencables > 0 {
            info!(
                non_referencables = originale.non_referencables,
                "circle_suivi_lignes_non_referencables"
            );
        }
        Tentative::Fait(
            Bilan::Reporte {
                retires: a_retirer.len(),
                ajoutes: a_ajouter.len(),
                reordonne,
                renomme,
                ecart,
            },
            Some(cercle),
        )
    }

    /// La boucle de fond : un tour toutes les [`PERIODE`], ou dès un réveil.
    pub async fn tourner(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(PERIODE) => {}
                _ = self.reveil.notified() => {}
            }
            self.tour().await;
        }
    }
}

fn cle_d_item(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

fn version_en_texte(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        autre => autre.to_string(),
    }
}

// Routes ---------------------------------------------------------------------

pub fn router(suivi: Arc<Suivi>) -> Router<()> {
    Router::new()
        .route(
            "/playlists/{id}/follow",
            put(suivre).get(etat).delete(ne_plus_suivre),
        )
        .route("/playlists/{id}/follow/sync", post(synchroniser))
        .with_state(suivi)
}

/// `PUT /playlists/{id}/follow` `{ playlist_id, circle_id?, circle_name? }` :
/// la playlist de cercle
/// `{id}` suit désormais la playlist locale `playlist_id` (du profil actif).
/// Le premier report part aussitôt ; la réponse dit le lien et le report.
async fn suivre(State(s): State<Arc<Suivi>>, Path(id): Path<String>, corps: Bytes) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let demande = serde_json::from_slice::<Value>(&corps).unwrap_or(Value::Null);
    let backend = s.relais.backend().clone();
    let profil = profil_actif_de(&backend);
    let source = demande["playlist_id"].as_i64().and_then(|pid| {
        PlaylistRepo::with_backend(backend.clone())
            .get_for_profile(pid, profil)
            .ok()
            .flatten()
            .map(|p| (pid, p.name))
    });
    let Some((local, nom)) = source else {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "code": CODE_SOURCE_INCONNUE }),
        );
    };
    let nom_du_cercle = demande["circle_name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    let (l, bilan) = s
        .suivre(
            &id,
            local,
            &nom,
            demande["circle_id"].clone(),
            nom_du_cercle,
        )
        .await;
    Json(json!({ "follow": l.pour_le_client(), "sync": bilan.en_json() })).into_response()
}

/// `GET /playlists/{id}/follow` : le lien, ou `{ following: false }`.
async fn etat(State(s): State<Arc<Suivi>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    match lien(&s.reglages(), &id) {
        Some(l) => Json(l.pour_le_client()).into_response(),
        None => Json(json!({ "following": false })).into_response(),
    }
}

/// `DELETE /playlists/{id}/follow` : « Ne plus suivre ».
async fn ne_plus_suivre(State(s): State<Arc<Suivi>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    if s.ne_plus_suivre(&id) {
        Json(json!({ "ok": true, "following": false })).into_response()
    } else {
        refus(StatusCode::NOT_FOUND, json!({ "code": CODE_NON_SUIVIE }))
    }
}

/// `POST /playlists/{id}/follow/sync` : « Synchroniser maintenant ». Relit le
/// cercle même si l'originale n'a pas changé.
async fn synchroniser(State(s): State<Arc<Suivi>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    if lien(&s.reglages(), &id)
        .filter(|l| l.stopped_at.is_none())
        .is_none()
    {
        return refus(StatusCode::NOT_FOUND, json!({ "code": CODE_NON_SUIVIE }));
    }
    let bilan = s.reporter_avec(&id, true).await;
    let l = lien(&s.reglages(), &id);
    Json(json!({
        "follow": l.map(|l| l.pour_le_client()),
        "sync": bilan.en_json(),
    }))
    .into_response()
}

/// `GET /playlists/{id}` : la fiche d'une playlist de cercle suivie porte
/// `follow` (« Suit la playlist « … » », date du dernier report,
/// avertissement). Les autres réponses passent telles quelles.
pub(crate) fn avec_le_suivi(reglages: &SettingsRepo, id: &str, issue: Issue) -> Response {
    let Some(l) = lien(reglages, id) else {
        return en_reponse(issue);
    };
    match issue {
        Issue::Reponse {
            statut: 200,
            corps,
            retry_after,
            etag,
        } => match serde_json::from_slice::<Value>(&corps) {
            Ok(Value::Object(mut p)) => {
                p.insert("follow".into(), l.pour_le_client());
                let corps = serde_json::to_vec(&Value::Object(p)).unwrap_or(corps);
                en_reponse(Issue::Reponse {
                    statut: 200,
                    corps,
                    retry_after,
                    etag,
                })
            }
            _ => en_reponse(Issue::Reponse {
                statut: 200,
                corps,
                retry_after,
                etag,
            }),
        },
        autre => en_reponse(autre),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tune-server` (`routes/playlists.rs`, badge « Partagée avec un
    /// cercle ») lit ces clés du lien persisté, sans dépendre du greffon.
    #[test]
    fn le_lien_persiste_les_cles_que_lit_le_serveur() {
        let l = Lien {
            circle_playlist_id: "6".into(),
            local_playlist_id: 3,
            circle_id: json!(1),
            circle_name: Some("Famille".into()),
            stopped_at: Some("2026-10-21T10:00:00Z".into()),
            last_sync: Some("2026-10-21T09:00:00Z".into()),
            ..Lien::default()
        };
        let v = l.en_json();
        assert_eq!(v["local_playlist_id"], 3);
        assert_eq!(v["circle_id"], 1);
        assert_eq!(v["circle_name"], "Famille");
        assert_eq!(v["stopped_at"], "2026-10-21T10:00:00Z");
        assert_eq!(v["last_sync"], "2026-10-21T09:00:00Z");
        assert_eq!(Lien::depuis_json(&v), Some(l));
    }
}
