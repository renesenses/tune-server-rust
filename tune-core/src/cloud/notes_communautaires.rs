//! Notes et écoutes de la communauté (#6545) : « ★ 4,2 (37 notes) · 1 240
//! écoutes dans la communauté » sur la fiche d'un album.
//!
//! Deux sens :
//!
//! 1. **Ce serveur contribue** ([`passe`]) — seulement si l'utilisateur a
//!    coché « Partager mes notes et mes écoutes avec la communauté
//!    (anonymement) » ([`CLE_PARTAGE`], **désactivé par défaut**, distinct de
//!    l'envoi « Mes écoutes »). L'état du consentement est poussé au site à
//!    chaque changement (`PUT /api/v1/community/consent`) : c'est le site qui
//!    l'horodate, par compte, et qui ne compte que ce qu'il reçoit ensuite.
//!    Avec le consentement, l'**instantané complet** des notes d'album
//!    (`album_ratings`, 1 à 5, tous profils) part à chaque changement
//!    (`PUT /api/v1/community/ratings`) : ajout, changement et retrait d'une
//!    note tiennent dans le même geste, idempotent. Les écoutes, elles, sont
//!    déjà sur le compte par « Mes écoutes » (`ecoutes_au_compte`) : le site
//!    les compte pour la communauté quand le compte a consenti.
//! 2. **Ce serveur lit** ([`agregats_d_album`]) — les agrégats publics d'un
//!    album, par `GET /api/v1/community/albums`, gardés
//!    [`CACHE_SECONDES`] en mémoire. Le site ne rend rien sous trois
//!    personnes distinctes et aucune donnée individuelle.
//!
//! Ce qui part d'une note : l'empreinte du profil
//! ([`super::ecoutes_au_compte::empreinte_de_profil`]), l'artiste et le titre
//! de l'album, la note. Jamais le commentaire de la note, ni le numéro de
//! profil, ni l'identifiant local de l'album.
//!
//! Même chaîne que les écoutes : compte relié lu par
//! [`super::sauvegarde_config::compte`], jeton de liaison en
//! `X-Tune-Server-Token`, reliaison unique sur jeton refusé, rafraîchissement
//! unique de la session sur 401.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::cloud::consent::est_vrai;
use crate::cloud::ecoutes_au_compte::{empreinte_de_profil, motif, relier};
use crate::cloud::rate_limit::{self, AppelCloud, CloudScope};
use crate::cloud::sauvegarde_config::{self as sc, Compte};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

/// Le consentement, réglage local écrit par le client web (`PATCH
/// /system/config`), publié par `GET /system/config` avec [`PARTAGE_DEFAUT`].
pub const CLE_PARTAGE: &str = "cloud_community_share_enabled";
/// Désactivé par défaut : opt-in strict (décision de Bertrand).
pub const PARTAGE_DEFAUT: bool = false;
/// Dernier état du consentement ACCEPTÉ par le site (`true`/`false`).
pub const CLE_CONSENTEMENT_TRANSMIS: &str = "cloud_community_share_synced";
/// Empreinte du dernier instantané de notes accepté par le site.
pub const CLE_EMPREINTE_NOTES: &str = "cloud_community_ratings_hash";

const CHEMIN_CONSENTEMENT: &str = "/api/v1/community/consent";
const CHEMIN_NOTES: &str = "/api/v1/community/ratings";
const CHEMIN_ALBUMS: &str = "/api/v1/community/albums";

/// Notes au plus dans un instantané (celles que le site accepte).
pub const NOTES_MAX: i64 = 10_000;
/// Durée de vie d'un agrégat lu au site.
pub const CACHE_SECONDES: u64 = 6 * 3600;
/// Cadence de la tâche de fond.
pub const CADENCE_SECONDES: u64 = 300;

/// L'utilisateur a-t-il consenti ? Absent, illisible ou autre : non.
pub fn partage_active(settings: &SettingsRepo) -> bool {
    settings
        .get(CLE_PARTAGE)
        .ok()
        .flatten()
        .map(|v| est_vrai(&v))
        .unwrap_or(PARTAGE_DEFAUT)
}

/// Ce qu'un passage a fait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// Rien n'est parti, et c'est normal : `account_not_linked`,
    /// `disabled`, `unchanged`.
    Rien(&'static str),
    /// Le consentement (et, s'il est donné, l'instantané) a été transmis.
    Transmis {
        consentement: Option<bool>,
        notes: Option<usize>,
    },
    /// Un envoi a échoué : rien n'est noté, il repartira.
    Echec(String),
}

/// Un appel au site au nom du compte, avec la reliaison et le
/// rafraîchissement uniques de la chaîne des écoutes.
struct Session<'a> {
    backend: &'a Arc<dyn DbBackend>,
    http: &'a reqwest::Client,
    settings: SettingsRepo,
    compte: Compte,
    liaison_tentee: bool,
    session_rafraichie: bool,
}

enum Reponse {
    Ok(Value),
    Refus { statut: u16, code: Option<String> },
    Panne(&'static str),
}

impl Session<'_> {
    async fn appeler(
        &mut self,
        methode: reqwest::Method,
        chemin: &str,
        requete: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        portee: CloudScope,
    ) -> Reponse {
        loop {
            let base = self
                .http
                .request(methode.clone(), format!("{}{chemin}", self.compte.base))
                .bearer_auth(&self.compte.acces)
                .header(
                    crate::cloud::library_sync::EN_TETE_JETON_DE_SERVEUR,
                    self.compte.liaison.clone().unwrap_or_default(),
                )
                .header(reqwest::header::ACCEPT, "application/json")
                .timeout(Duration::from_secs(20));
            match rate_limit::appeler(&self.settings, portee, requete(base)).await {
                AppelCloud::Retenu(_) => return Reponse::Panne("rate_limited"),
                AppelCloud::Erreur(_) => return Reponse::Panne("network"),
                AppelCloud::Reponse(r) if r.status().is_success() => {
                    return Reponse::Ok(r.json().await.unwrap_or(Value::Null));
                }
                AppelCloud::Reponse(r) => {
                    let statut = r.status().as_u16();
                    let code = motif(&r.text().await.unwrap_or_default());
                    let liaison_refusee = matches!(
                        (statut, code.as_deref()),
                        (401, Some("server_token_invalid")) | (404, _)
                    );
                    if liaison_refusee && !self.liaison_tentee {
                        self.liaison_tentee = true;
                        if relier(self.backend, self.http, &self.settings, &mut self.compte).await {
                            continue;
                        }
                    } else if statut == 401 && !self.session_rafraichie {
                        self.session_rafraichie = true;
                        if let Some(c) = sc::rafraichir(&self.settings).await {
                            self.compte = c;
                            continue;
                        }
                    }
                    return Reponse::Refus { statut, code };
                }
            }
        }
    }
}

fn raison(r: &Reponse) -> String {
    match r {
        Reponse::Ok(_) => "ok".into(),
        Reponse::Panne(p) => (*p).into(),
        Reponse::Refus {
            statut,
            code: Some(c),
        } => format!("HTTP {statut} {c}"),
        Reponse::Refus { statut, code: None } => format!("HTTP {statut}"),
    }
}

/// L'instantané des notes d'album de ce serveur, tel qu'il part : trié, pour
/// que la même base donne le même corps (et la même empreinte).
///
/// Ni le commentaire (`note`), ni le numéro de profil, ni l'identifiant local
/// de l'album ne sont lus pour partir.
pub fn instantane_des_notes(
    backend: &Arc<dyn DbBackend>,
    server_id: &str,
) -> Result<Vec<Value>, String> {
    let lignes = backend.query_many(
        "SELECT CAST(r.profile_id AS BIGINT), a.title, ar.name, CAST(r.rating AS BIGINT) \
         FROM album_ratings r \
         JOIN albums a ON a.id = r.album_id \
         LEFT JOIN artists ar ON ar.id = a.artist_id \
         WHERE r.rating BETWEEN 1 AND 5 \
         ORDER BY r.album_id, r.profile_id LIMIT ?",
        &[&NOTES_MAX as &dyn ToSqlValue],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|r| {
            let album = r
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())?;
            let note = r.get(3).and_then(|v| v.as_i64())?;
            let profil = r
                .first()
                .and_then(|v| v.as_i64())
                .map(|p| empreinte_de_profil(server_id, p));
            Some(json!({
                "profile": profil,
                "artist": r.get(2).and_then(|v| v.as_string()),
                "album": album,
                "rating": note,
            }))
        })
        .collect())
}

fn empreinte(valeur: &Value) -> String {
    format!("{:x}", Sha256::digest(valeur.to_string().as_bytes()))
}

/// Un passage : transmet le consentement s'il a changé, puis, avec le
/// consentement, l'instantané des notes s'il a changé.
pub async fn passe(backend: &Arc<dyn DbBackend>, http: &reqwest::Client) -> Issue {
    let settings = SettingsRepo::with_backend(backend.clone());
    let voulu = partage_active(&settings);
    let transmis = settings
        .get(CLE_CONSENTEMENT_TRANSMIS)
        .ok()
        .flatten()
        .map(|v| est_vrai(&v));
    // Pas de consentement, et le site ne croit pas en avoir un (jamais
    // transmis, ou retrait déjà transmis) : il n'y a rien à lui dire.
    if !voulu && transmis != Some(true) {
        return Issue::Rien("disabled");
    }
    let Ok(compte) = sc::compte(&settings) else {
        return Issue::Rien("account_not_linked");
    };
    let mut session = Session {
        backend,
        http,
        settings: SettingsRepo::with_backend(backend.clone()),
        compte,
        liaison_tentee: false,
        session_rafraichie: false,
    };
    if session.compte.liaison.is_none() {
        session.liaison_tentee = true;
        if !relier(backend, http, &settings, &mut session.compte).await {
            return Issue::Echec("server_not_linked".into());
        }
    }
    let server_id = session.compte.server_id.clone();

    let mut consentement = None;
    if transmis != Some(voulu) {
        let corps = json!({ "server_id": server_id, "enabled": voulu });
        let r = session
            .appeler(
                reqwest::Method::PUT,
                CHEMIN_CONSENTEMENT,
                |q| q.json(&corps),
                CloudScope::CommunityShareWrite,
            )
            .await;
        if !matches!(r, Reponse::Ok(_)) {
            debug!(raison = %raison(&r), "community_consent_not_sent");
            return Issue::Echec(raison(&r));
        }
        settings
            .set(
                CLE_CONSENTEMENT_TRANSMIS,
                if voulu { "true" } else { "false" },
            )
            .ok();
        // Retiré, le consentement efface les notes partagées chez le site : le
        // prochain consentement devra tout renvoyer.
        settings.delete(CLE_EMPREINTE_NOTES).ok();
        info!(enabled = voulu, "community_consent_sent");
        consentement = Some(voulu);
    }
    if !voulu {
        return Issue::Transmis {
            consentement,
            notes: None,
        };
    }

    let notes = match instantane_des_notes(backend, &server_id) {
        Ok(n) => n,
        Err(e) => return Issue::Echec(format!("album_ratings: {e}")),
    };
    let corps = json!({ "server_id": server_id, "ratings": notes });
    let signature = empreinte(&corps);
    if settings.get(CLE_EMPREINTE_NOTES).ok().flatten().as_deref() == Some(signature.as_str()) {
        return match consentement {
            Some(_) => Issue::Transmis {
                consentement,
                notes: None,
            },
            None => Issue::Rien("unchanged"),
        };
    }
    let r = session
        .appeler(
            reqwest::Method::PUT,
            CHEMIN_NOTES,
            |q| q.json(&corps),
            CloudScope::CommunityShareWrite,
        )
        .await;
    match r {
        Reponse::Ok(_) => {
            settings.set(CLE_EMPREINTE_NOTES, &signature).ok();
            let n = notes.len();
            info!(notes = n, "community_ratings_sent");
            Issue::Transmis {
                consentement,
                notes: Some(n),
            }
        }
        Reponse::Refus {
            statut: 403,
            code: Some(ref c),
        } if c == "consent_required" => {
            // Le site a perdu (ou n'a jamais eu) le consentement : il sera
            // retransmis au prochain passage.
            settings.delete(CLE_CONSENTEMENT_TRANSMIS).ok();
            warn!("community_ratings_consent_required");
            Issue::Echec("consent_required".into())
        }
        autre => Issue::Echec(raison(&autre)),
    }
}

/// La tâche de fond : un passage toutes les [`CADENCE_SECONDES`], deux
/// minutes après le démarrage.
pub fn spawn(backend: Arc<dyn DbBackend>, http: reqwest::Client) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(CADENCE_SECONDES));
        loop {
            ticker.tick().await;
            if let Issue::Echec(e) = passe(&backend, &http).await {
                debug!(raison = %e, "community_pass_failed");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Lecture des agrégats d'un album
// ---------------------------------------------------------------------------

type Cache = HashMap<String, (Instant, Value)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn cle_de_cache(base: &str, artiste: &str, album: &str) -> String {
    format!(
        "{base}\x1f{}\x1f{}",
        artiste.trim().to_lowercase(),
        album.trim().to_lowercase()
    )
}

/// Les agrégats communautaires d'un album local, pour l'interface :
/// `{available, rating: {average, count} | null, listens: {count, listeners} | null}`.
///
/// `available: false` quand rien n'a pu être demandé ou lu (album inconnu,
/// serveur non relié, site injoignable) : l'interface n'affiche alors rien.
/// Une réponse lue est gardée [`CACHE_SECONDES`] ; un échec ne l'est pas.
pub async fn agregats_d_album(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    album_id: i64,
) -> Value {
    let indisponible = |raison: &str| json!({ "available": false, "reason": raison });
    let Ok(Some(ligne)) = backend.query_one(
        "SELECT a.title, ar.name FROM albums a LEFT JOIN artists ar ON ar.id = a.artist_id \
         WHERE a.id = ?",
        &[&album_id as &dyn ToSqlValue],
    ) else {
        return indisponible("album_not_found");
    };
    let Some(album) = ligne
        .first()
        .and_then(|v| v.as_string())
        .filter(|s| !s.trim().is_empty())
    else {
        return indisponible("album_not_found");
    };
    let artiste = ligne.get(1).and_then(|v| v.as_string()).unwrap_or_default();

    let settings = SettingsRepo::with_backend(backend.clone());
    let Ok(compte) = sc::compte(&settings) else {
        return indisponible("account_not_linked");
    };
    if compte.liaison.is_none() {
        return indisponible("account_not_linked");
    }
    let cle = cle_de_cache(&compte.base, &artiste, &album);
    if let Some((quand, valeur)) = CACHE.lock().ok().and_then(|c| c.get(&cle).cloned())
        && quand.elapsed() < Duration::from_secs(CACHE_SECONDES)
    {
        return valeur;
    }

    let server_id = compte.server_id.clone();
    let mut session = Session {
        backend,
        http,
        settings,
        compte,
        liaison_tentee: false,
        session_rafraichie: false,
    };
    let requete = [
        ("server_id", server_id.as_str()),
        ("artist", artiste.as_str()),
        ("album", album.as_str()),
    ];
    let r = session
        .appeler(
            reqwest::Method::GET,
            CHEMIN_ALBUMS,
            |q| q.query(&requete),
            CloudScope::CommunityAlbumsRead,
        )
        .await;
    let Reponse::Ok(corps) = r else {
        debug!(raison = %raison(&r), "community_album_unavailable");
        return indisponible("site_unavailable");
    };
    let valeur = json!({
        "available": true,
        "rating": corps.get("rating").cloned().filter(|v| v.is_object()),
        "listens": corps.get("listens").cloned().filter(|v| v.is_object()),
    });
    if let Ok(mut c) = CACHE.lock() {
        c.insert(cle, (Instant::now(), valeur.clone()));
    }
    valeur
}

#[cfg(test)]
#[path = "notes_communautaires_tests.rs"]
mod tests;
