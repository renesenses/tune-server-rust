//! Favoris de service en MIROIR (#5997, rc4).
//!
//! # La décision
//!
//! Bertrand, 08/10/2026, confirmé le 09/10 (fil 2186, FabienM) : pour un
//! service en miroir (Qobuz, Tidal — [`StreamingService::favoris_miroir`]),
//! la vérité est CHEZ LE SERVICE, dans les deux sens :
//!
//! * un cœur posé ou retiré dans Tune est propagé au service PAR LE SERVEUR ;
//! * un favori posé ou retiré dans l'application du service apparaît ou
//!   disparaît dans Favoris au rafraîchissement, périodique et à l'ouverture,
//!   avec un cache court ;
//! * ces favoris sont COMMUNS à tous les profils, puisque le compte du service
//!   l'est. Les favoris de la bibliothèque locale restent propres à chacun.
//!
//! # Comment « commun » tient sans réécrire les lecteurs
//!
//! Par RÉPLICATION : une écriture en miroir pose ou retire la ligne de
//! `streaming_favorites` pour chaque profil. La liste, le tri, le rang manuel,
//! les règles intelligentes, les étiquettes et le marquage IA lisent tous la
//! table par profil ; ils restent justes sans être touchés, et le rang manuel
//! reste propre à chaque profil.
//!
//! # Ce qui ne se perd jamais
//!
//! La colonne `miroir_etat` distingue ce que le service a confirmé
//! (`synchro`) de ce que Tune attend encore (`ajout_en_attente`,
//! `retrait_en_attente`). Un échec chez le service — réseau, jeton expiré,
//! refus — laisse la ligne en attente avec son motif (`miroir_erreur`), et le
//! rafraîchissement suivant retente. Une lecture ratée chez le service ne
//! retire RIEN : seule une ligne `synchro` que le service a cessé de
//! nommer, dans une lecture réussie, est retirée.
//!
//! Les lignes d'avant la rc4 (`miroir_etat` NULL) sont adoptées au premier
//! rafraîchissement : `synchro` si le service les connaît, sinon
//! `ajout_en_attente` puis poussées — un cœur posé dans Tune vaut désormais un
//! favori chez le service. Aucune n'est effacée.
//!
//! # Playlists
//!
//! Poussées, jamais relues. Chez Qobuz une playlist ne passe pas par
//! `/favorite/create` mais par `/playlist/subscribe` (#2370,
//! `MOTIF_PLAYLIST_HORS_FAVORITE_CREATE`) : le miroir appelle le même
//! `add_favorite("playlists")` que le connecteur route déjà là. Aucune liste
//! de playlists suivies n'étant établie, une ligne `playlist` n'est jamais
//! retirée par un rafraîchissement.
//!
//! Contrat d'API complet : description de la PR #5997.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::favorites_import::{Entree, lire_les_favoris};
use super::traits::StreamingService;
use crate::db::backend::{DbBackend, SqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::db::streaming_favorites_repo::StreamingFavoritesRepo;
use crate::streaming::favorites_identity::identite_de_favori;

/// Le service a confirmé ce favori.
pub const ETAT_SYNCHRO: &str = "synchro";
/// Posé dans Tune, pas encore confirmé chez le service.
pub const ETAT_AJOUT_EN_ATTENTE: &str = "ajout_en_attente";
/// Retiré dans Tune, pas encore confirmé chez le service — masqué de la liste.
pub const ETAT_RETRAIT_EN_ATTENTE: &str = "retrait_en_attente";

/// Cache court de la liste : au-delà, l'ouverture des Favoris rafraîchit.
pub const TTL_DEFAUT_S: u64 = 60;
/// Période du rafraîchissement de fond.
pub const PERIODE_DEFAUT_S: u64 = 300;
/// Délai accordé au service pour UNE écriture.
pub const DELAI_ECRITURE_SERVICE: Duration = Duration::from_secs(10);

/// Le service que le miroir manipule, tel que le registre le tient.
pub type ServiceArc = Arc<RwLock<Box<dyn StreamingService>>>;

fn duree_env(nom: &str, defaut: u64) -> u64 {
    std::env::var(nom)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(defaut)
}

/// `TUNE_FAVORIS_MIROIR_TTL_S`, 60 s par défaut.
pub fn ttl() -> Duration {
    Duration::from_secs(duree_env("TUNE_FAVORIS_MIROIR_TTL_S", TTL_DEFAUT_S))
}

/// `TUNE_FAVORIS_MIROIR_PERIODE_S`, 300 s par défaut ; `0` coupe la veille.
pub fn periode() -> Option<Duration> {
    match duree_env("TUNE_FAVORIS_MIROIR_PERIODE_S", PERIODE_DEFAUT_S) {
        0 => None,
        s => Some(Duration::from_secs(s)),
    }
}

/// Le type de favori du client (singulier) en type du connecteur (pluriel).
pub fn type_service(item_type: &str) -> Option<&'static str> {
    match item_type {
        "track" => Some("tracks"),
        "album" => Some("albums"),
        "artist" => Some("artists"),
        "playlist" => Some("playlists"),
        _ => None,
    }
}

/// Un favori tel que le cœur de Tune le décrit.
#[derive(Debug, Clone, Default)]
pub struct FavoriMiroir {
    pub item_type: String,
    pub service_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub ai_generated: Option<bool>,
    pub isrc: Option<String>,
    /// Date de mise en favori chez le service, quand elle est connue.
    pub created_at: Option<String>,
}

impl From<Entree> for FavoriMiroir {
    fn from(e: Entree) -> Self {
        FavoriMiroir {
            item_type: e.item_type.to_string(),
            service_id: e.service_id,
            title: e.title,
            artist: e.artist,
            album: e.album,
            cover_url: e.cover_url,
            ai_generated: e.ai_generated,
            isrc: e.isrc,
            created_at: e.created_at,
        }
    }
}

/// Ce qu'est devenue une écriture demandée dans Tune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Propagation {
    /// Le service a confirmé.
    Propage,
    /// Le service n'a pas suivi : la ligne est gardée en attente, avec ce motif.
    EnAttente(String),
}

impl Propagation {
    /// Le champ `miroir` des réponses de `/favorites/streaming/add|remove`.
    pub fn en_json(&self, service: &str) -> serde_json::Value {
        match self {
            Propagation::Propage => serde_json::json!({"service": service, "statut": "propage"}),
            Propagation::EnAttente(e) => {
                serde_json::json!({"service": service, "statut": "en_attente", "erreur": e})
            }
        }
    }
}

/// Le compte d'un rafraîchissement. Reprend les champs de la reprise
/// (`RepriseFavoris`) pour que `POST …/streaming/sync` garde sa forme.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct BilanMiroir {
    pub lus: usize,
    pub ajoutes: usize,
    pub deja_presents: usize,
    pub echecs: usize,
    pub redates: usize,
    /// Favoris retirés de Tune parce que le service ne les nomme plus.
    pub retires: usize,
    /// Écritures en attente que le service vient de confirmer.
    pub pousses: usize,
    /// Écritures encore en attente après ce passage.
    pub en_attente: usize,
    /// Dernier motif d'échec rencontré, s'il y en a un.
    pub erreur: Option<String>,
}

// ---------------------------------------------------------------------------
// État par service : dernier rafraîchissement, statut, verrou.
// ---------------------------------------------------------------------------

/// Ce que `GET /profiles/{id}/favorites/streaming/miroir` rend par service.
#[derive(Debug, Clone, Serialize)]
pub struct EtatMiroir {
    pub miroir: bool,
    pub dernier_rafraichissement: Option<String>,
    /// `jamais`, `ok` ou `echec`.
    pub statut: &'static str,
    pub erreur: Option<String>,
    pub en_attente: usize,
    #[serde(skip)]
    instant: Option<Instant>,
    #[serde(skip)]
    perime: bool,
}

impl Default for EtatMiroir {
    fn default() -> Self {
        EtatMiroir {
            miroir: true,
            dernier_rafraichissement: None,
            statut: "jamais",
            erreur: None,
            en_attente: 0,
            instant: None,
            perime: false,
        }
    }
}

fn etats() -> &'static Mutex<HashMap<String, EtatMiroir>> {
    static ETATS: OnceLock<Mutex<HashMap<String, EtatMiroir>>> = OnceLock::new();
    ETATS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn verrous() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static VERROUS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    VERROUS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Un seul rafraîchissement à la fois par service.
fn verrou(service: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut v = verrous().lock().unwrap_or_else(|p| p.into_inner());
    v.entry(service.to_string()).or_default().clone()
}

/// L'état connu du miroir d'un service.
pub fn etat(service: &str) -> EtatMiroir {
    etats()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(service)
        .cloned()
        .unwrap_or_default()
}

/// Marque le miroir PÉRIMÉ : la prochaine lecture de la liste rafraîchit.
/// Appelé après une écriture directe chez le service
/// (`POST|DELETE /streaming/{service}/favorites/{type}/{id}`).
pub fn invalider(service: &str) {
    let mut e = etats().lock().unwrap_or_else(|p| p.into_inner());
    e.entry(service.to_string()).or_default().perime = true;
}

/// Le dernier rafraîchissement date-t-il de plus de `ttl` (ou jamais) ?
pub fn est_perime(service: &str, ttl: Duration) -> bool {
    let e = etat(service);
    e.perime || e.instant.is_none_or(|i| i.elapsed() >= ttl)
}

fn noter_rafraichissement(service: &str, bilan: &BilanMiroir) {
    let mut e = etats().lock().unwrap_or_else(|p| p.into_inner());
    let etat = e.entry(service.to_string()).or_default();
    etat.instant = Some(Instant::now());
    etat.perime = false;
    etat.dernier_rafraichissement =
        Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    etat.statut = if bilan.echecs > 0 { "echec" } else { "ok" };
    etat.erreur = bilan.erreur.clone();
    etat.en_attente = bilan.en_attente;
}

fn noter_en_attente(service: &str, backend: &Arc<dyn DbBackend>) {
    let n = compter_en_attente(backend, service);
    let mut e = etats().lock().unwrap_or_else(|p| p.into_inner());
    e.entry(service.to_string()).or_default().en_attente = n;
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Les placeholders du moteur, numérotés à partir de 1 dans l'ordre
/// d'apparition — SQLite ignore l'indice (`?`), on lie donc dans l'ordre.
fn ph(backend: &Arc<dyn DbBackend>, n: usize) -> String {
    match backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

fn texte(v: Option<&SqlValue>) -> Option<String> {
    v.and_then(|x| x.as_string())
}

/// Tous les profils du foyer, l'appelant compris même si la table est vide.
pub fn profils(backend: &Arc<dyn DbBackend>, appelant: i64) -> Vec<i64> {
    let mut ids: Vec<i64> = backend
        .query_many_strong("SELECT id FROM profiles", &[])
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            r.first().and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_string().and_then(|s| s.trim().parse().ok()))
            })
        })
        .collect();
    if !ids.contains(&appelant) {
        ids.push(appelant);
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Pose l'état du miroir d'un favori, TOUS profils confondus.
fn poser_etat(
    backend: &Arc<dyn DbBackend>,
    service: &str,
    item_type: &str,
    service_id: &str,
    etat: &str,
    erreur: Option<&str>,
) -> Result<usize, String> {
    let sql = format!(
        "UPDATE streaming_favorites SET miroir_etat = {}, miroir_erreur = {} \
         WHERE service = {} AND item_type = {} AND service_id = {}",
        ph(backend, 1),
        ph(backend, 2),
        ph(backend, 3),
        ph(backend, 4),
        ph(backend, 5)
    );
    let cle = identite_de_favori(service_id);
    let id: &str = cle.as_ref();
    backend.execute(&sql, &[&etat, &erreur, &service, &item_type, &id])
}

/// Retire un favori de TOUS les profils.
fn retirer_partout(
    backend: &Arc<dyn DbBackend>,
    service: &str,
    item_type: &str,
    service_id: &str,
) -> Result<usize, String> {
    let sql = format!(
        "DELETE FROM streaming_favorites WHERE service = {} AND item_type = {} AND service_id = {}",
        ph(backend, 1),
        ph(backend, 2),
        ph(backend, 3)
    );
    let cle = identite_de_favori(service_id);
    let id: &str = cle.as_ref();
    backend.execute(&sql, &[&service, &item_type, &id])
}

/// Pose un favori pour CHAQUE profil, dans l'état donné.
fn ecrire_partout(
    backend: &Arc<dyn DbBackend>,
    profils: &[i64],
    service: &str,
    fav: &FavoriMiroir,
    etat: &str,
    erreur: Option<&str>,
) -> Result<(), String> {
    let repo = StreamingFavoritesRepo::with_backend(backend.clone());
    for &pid in profils {
        repo.add_date(
            pid,
            &fav.item_type,
            service,
            &fav.service_id,
            fav.title.as_deref(),
            fav.artist.as_deref(),
            fav.album.as_deref(),
            fav.cover_url.as_deref(),
            fav.created_at.as_deref(),
        )?;
        if let Some(ia) = fav.ai_generated {
            let _ = repo.poser_ia(pid, &fav.item_type, service, &fav.service_id, ia);
        }
    }
    poser_etat(
        backend,
        service,
        &fav.item_type,
        &fav.service_id,
        etat,
        erreur,
    )?;
    if let Some(isrc) = fav.isrc.as_deref().filter(|i| !i.trim().is_empty()) {
        let sql = format!(
            "UPDATE streaming_favorites SET isrc = {} \
             WHERE service = {} AND item_type = {} AND service_id = {}",
            ph(backend, 1),
            ph(backend, 2),
            ph(backend, 3),
            ph(backend, 4)
        );
        let cle = identite_de_favori(&fav.service_id);
        let id: &str = cle.as_ref();
        let isrc = isrc.trim().to_uppercase();
        backend.execute(&sql, &[&isrc, &service, &fav.item_type, &id])?;
    }
    Ok(())
}

/// Combien d'écritures attendent encore le service.
pub fn compter_en_attente(backend: &Arc<dyn DbBackend>, service: &str) -> usize {
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT DISTINCT item_type, service_id FROM streaming_favorites \
         WHERE service = {} AND miroir_etat IN ('{ETAT_AJOUT_EN_ATTENTE}', '{ETAT_RETRAIT_EN_ATTENTE}')) en_attente",
        ph(backend, 1)
    );
    backend
        .query_one_strong(&sql, &[&service])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0) as usize
}

/// Donne à `profile_id` les favoris en miroir que d'autres profils ont et
/// qu'il n'a pas — un profil créé après coup, ou une écriture faite quand il
/// n'existait pas encore. Local, sans réseau : appelé à chaque lecture.
pub fn aligner_profil(
    backend: &Arc<dyn DbBackend>,
    profile_id: i64,
    services: &[String],
) -> Result<usize, String> {
    let mut poses = 0usize;
    for service in services {
        let manquants = format!(
            "SELECT DISTINCT a.item_type, a.service_id FROM streaming_favorites a \
             WHERE a.service = {} AND a.profile_id <> {} \
               AND a.miroir_etat IN ('{ETAT_SYNCHRO}', '{ETAT_AJOUT_EN_ATTENTE}') \
               AND NOT EXISTS (SELECT 1 FROM streaming_favorites b WHERE b.profile_id = {} \
                   AND b.item_type = a.item_type AND b.service = a.service AND b.service_id = a.service_id)",
            ph(backend, 1),
            ph(backend, 2),
            ph(backend, 3)
        );
        let lignes = backend.query_many_strong(&manquants, &[service, &profile_id, &profile_id])?;
        let copie = format!(
            "INSERT INTO streaming_favorites \
             (profile_id, item_type, service, service_id, title, artist, album, cover_url, created_at, \
              first_seen_at, ai_generated, album_ref, miroir_etat, miroir_erreur, isrc) \
             SELECT {}, item_type, service, service_id, title, artist, album, cover_url, created_at, \
              first_seen_at, ai_generated, album_ref, miroir_etat, miroir_erreur, isrc \
             FROM streaming_favorites WHERE service = {} AND item_type = {} AND service_id = {} \
               AND profile_id <> {} AND id = (SELECT MIN(id) FROM streaming_favorites c \
                   WHERE c.service = {} AND c.item_type = {} AND c.service_id = {} AND c.profile_id <> {}) \
             ON CONFLICT (profile_id, item_type, service, service_id) DO NOTHING",
            ph(backend, 1),
            ph(backend, 2),
            ph(backend, 3),
            ph(backend, 4),
            ph(backend, 5),
            ph(backend, 6),
            ph(backend, 7),
            ph(backend, 8),
            ph(backend, 9)
        );
        for l in &lignes {
            let (Some(t), Some(id)) = (texte(l.first()), texte(l.get(1))) else {
                continue;
            };
            poses += backend.execute(
                &copie,
                &[
                    &profile_id,
                    service,
                    &t,
                    &id,
                    &profile_id,
                    service,
                    &t,
                    &id,
                    &profile_id,
                ],
            )?;
        }
    }
    Ok(poses)
}

// ---------------------------------------------------------------------------
// Écritures demandées dans Tune
// ---------------------------------------------------------------------------

/// Appelle le service pour UNE écriture, sous délai, verrou d'écriture tenu
/// le temps de l'appel seulement. `Err` porte le motif à afficher.
async fn ecrire_chez_le_service(
    arc: &ServiceArc,
    ajout: bool,
    fav_type: &str,
    service_id: &str,
) -> Result<(), String> {
    if !arc.read().await.utilisable().await {
        return Err("service non connecté".into());
    }
    let mut svc = arc.write().await;
    let appel = async {
        if ajout {
            svc.add_favorite(fav_type, service_id).await
        } else {
            svc.remove_favorite(fav_type, service_id).await
        }
    };
    match tokio::time::timeout(DELAI_ECRITURE_SERVICE, appel).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "le service n'a pas répondu en {} s",
            DELAI_ECRITURE_SERVICE.as_secs()
        )),
    }
}

/// Cœur posé dans Tune sur un favori d'un service en miroir : ajout CHEZ le
/// service, puis ligne pour tous les profils — `synchro` si le service a
/// confirmé, `ajout_en_attente` sinon. `Err` seulement si la base refuse.
pub async fn ajouter(
    arc: &ServiceArc,
    backend: &Arc<dyn DbBackend>,
    appelant: i64,
    fav: &FavoriMiroir,
) -> Result<Propagation, String> {
    let service = arc.read().await.name().to_string();
    let fav_type = type_service(&fav.item_type)
        .ok_or_else(|| format!("type de favori inconnu : {}", fav.item_type))?;
    let issue = ecrire_chez_le_service(arc, true, fav_type, &fav.service_id).await;
    let profils = profils(backend, appelant);
    let propagation = match issue {
        Ok(()) => {
            ecrire_partout(backend, &profils, &service, fav, ETAT_SYNCHRO, None)?;
            Propagation::Propage
        }
        Err(e) => {
            warn!(service = %service, item_type = %fav.item_type, service_id = %fav.service_id, erreur = %e, "favori_miroir_ajout_en_attente");
            ecrire_partout(
                backend,
                &profils,
                &service,
                fav,
                ETAT_AJOUT_EN_ATTENTE,
                Some(&e),
            )?;
            Propagation::EnAttente(e)
        }
    };
    noter_en_attente(&service, backend);
    Ok(propagation)
}

/// Cœur retiré dans Tune : retrait CHEZ le service, puis retrait de tous les
/// profils ; si le service n'a pas suivi, la ligne passe
/// `retrait_en_attente` (masquée, retentée).
pub async fn retirer(
    arc: &ServiceArc,
    backend: &Arc<dyn DbBackend>,
    item_type: &str,
    service_id: &str,
) -> Result<Propagation, String> {
    let service = arc.read().await.name().to_string();
    let fav_type =
        type_service(item_type).ok_or_else(|| format!("type de favori inconnu : {item_type}"))?;
    let propagation = match ecrire_chez_le_service(arc, false, fav_type, service_id).await {
        Ok(()) => {
            retirer_partout(backend, &service, item_type, service_id)?;
            Propagation::Propage
        }
        Err(e) => {
            warn!(service = %service, item_type, service_id, erreur = %e, "favori_miroir_retrait_en_attente");
            poser_etat(
                backend,
                &service,
                item_type,
                service_id,
                ETAT_RETRAIT_EN_ATTENTE,
                Some(&e),
            )?;
            Propagation::EnAttente(e)
        }
    };
    noter_en_attente(&service, backend);
    Ok(propagation)
}

// ---------------------------------------------------------------------------
// Rafraîchissement
// ---------------------------------------------------------------------------

/// `(service_id normalisé, état)` des lignes d'un service et d'un type, tous
/// profils confondus. Un retrait en attente l'emporte sur tout autre état :
/// l'auditeur a demandé le retrait.
fn etats_locaux(
    backend: &Arc<dyn DbBackend>,
    service: &str,
    item_type: &str,
) -> Result<HashMap<String, Option<String>>, String> {
    let sql = format!(
        "SELECT service_id, miroir_etat FROM streaming_favorites WHERE service = {} AND item_type = {}",
        ph(backend, 1),
        ph(backend, 2)
    );
    let mut out: HashMap<String, Option<String>> = HashMap::new();
    for r in backend.query_many_strong(&sql, &[&service, &item_type])? {
        let Some(id) = texte(r.first()) else { continue };
        let etat = texte(r.get(1));
        let rang = |e: &Option<String>| match e.as_deref() {
            Some(ETAT_RETRAIT_EN_ATTENTE) => 3,
            Some(ETAT_AJOUT_EN_ATTENTE) => 2,
            Some(ETAT_SYNCHRO) => 1,
            _ => 0,
        };
        match out.get(&id) {
            Some(deja) if rang(deja) >= rang(&etat) => {}
            _ => {
                out.insert(id, etat);
            }
        }
    }
    Ok(out)
}

/// Réconcilie un type relu AVEC SUCCÈS chez le service.
fn reconcilier(
    backend: &Arc<dyn DbBackend>,
    profils: &[i64],
    service: &str,
    item_type: &str,
    entrees: Vec<Entree>,
    bilan: &mut BilanMiroir,
) -> Result<(), String> {
    let repo = StreamingFavoritesRepo::with_backend(backend.clone());
    let locaux = etats_locaux(backend, service, item_type)?;
    let mut chez_service: HashSet<String> = HashSet::new();
    for e in entrees {
        if e.service_id.trim().is_empty() {
            continue;
        }
        bilan.lus += 1;
        let cle = identite_de_favori(&e.service_id).into_owned();
        chez_service.insert(cle.clone());
        let fav = FavoriMiroir::from(e);
        match locaux.get(&cle) {
            // Retrait demandé dans Tune, pas encore fait chez le service :
            // la poussée qui suit s'en charge.
            Some(Some(etat)) if etat == ETAT_RETRAIT_EN_ATTENTE => {}
            None => {
                ecrire_partout(backend, profils, service, &fav, ETAT_SYNCHRO, None)?;
                bilan.ajoutes += 1;
            }
            Some(_) => {
                ecrire_partout(backend, profils, service, &fav, ETAT_SYNCHRO, None)?;
                if let Some(date) = fav.created_at.as_deref() {
                    let mut redate = false;
                    for &pid in profils {
                        redate |= repo
                            .dater(pid, item_type, service, &fav.service_id, date)
                            .unwrap_or(false);
                    }
                    if redate {
                        bilan.redates += 1;
                    }
                }
                bilan.deja_presents += 1;
            }
        }
    }
    for (id, etat) in locaux {
        if chez_service.contains(&id) {
            continue;
        }
        match etat.as_deref() {
            // Le service l'a confirmé, puis ne le nomme plus : retiré dans
            // l'application du service.
            Some(ETAT_SYNCHRO) => {
                retirer_partout(backend, service, item_type, &id)?;
                bilan.retires += 1;
            }
            // Le retrait demandé dans Tune est fait.
            Some(ETAT_RETRAIT_EN_ATTENTE) => {
                retirer_partout(backend, service, item_type, &id)?;
            }
            // Ligne d'avant la rc4 que le service ne connaît pas : un cœur
            // posé dans Tune vaut un favori chez le service — à pousser.
            None => {
                poser_etat(
                    backend,
                    service,
                    item_type,
                    &id,
                    ETAT_AJOUT_EN_ATTENTE,
                    None,
                )?;
            }
            // Ajout en attente : la poussée qui suit s'en charge.
            Some(_) => {}
        }
    }
    Ok(())
}

/// Pousse au service les écritures en attente.
async fn pousser_en_attente(
    arc: &ServiceArc,
    backend: &Arc<dyn DbBackend>,
    service: &str,
    bilan: &mut BilanMiroir,
) -> Result<(), String> {
    let sql = format!(
        "SELECT DISTINCT item_type, service_id, miroir_etat FROM streaming_favorites \
         WHERE service = {} AND miroir_etat IN ('{ETAT_AJOUT_EN_ATTENTE}', '{ETAT_RETRAIT_EN_ATTENTE}')",
        ph(backend, 1)
    );
    let lignes = backend.query_many_strong(&sql, &[&service])?;
    for l in lignes {
        let (Some(item_type), Some(id), Some(etat)) =
            (texte(l.first()), texte(l.get(1)), texte(l.get(2)))
        else {
            continue;
        };
        let Some(fav_type) = type_service(&item_type) else {
            continue;
        };
        let ajout = etat == ETAT_AJOUT_EN_ATTENTE;
        match ecrire_chez_le_service(arc, ajout, fav_type, &id).await {
            Ok(()) => {
                if ajout {
                    poser_etat(backend, service, &item_type, &id, ETAT_SYNCHRO, None)?;
                } else {
                    retirer_partout(backend, service, &item_type, &id)?;
                }
                bilan.pousses += 1;
            }
            Err(e) => {
                poser_etat(backend, service, &item_type, &id, &etat, Some(&e))?;
                bilan.erreur = Some(e);
            }
        }
    }
    Ok(())
}

/// Rafraîchit le miroir d'UN service : relit, réconcilie, pousse. Voir
/// l'en-tête du module pour les règles. Le verrou de lecture du service n'est
/// tenu que pendant les lectures, celui d'écriture que le temps d'un appel :
/// la lecture en cours n'est pas bloquée par un rafraîchissement.
pub async fn rafraichir(
    arc: &ServiceArc,
    backend: &Arc<dyn DbBackend>,
    appelant: i64,
) -> BilanMiroir {
    let mut bilan = BilanMiroir::default();
    let (service, lectures) = {
        let svc = arc.read().await;
        let service = svc.name().to_string();
        if !svc.utilisable().await {
            bilan.echecs = 1;
            bilan.erreur = Some("service non connecté".into());
            drop(svc);
            bilan.en_attente = compter_en_attente(backend, &service);
            noter_rafraichissement(&service, &bilan);
            return bilan;
        }
        let mut lectures = Vec::new();
        for (fav_type, item_type) in [
            ("tracks", "track"),
            ("albums", "album"),
            ("artists", "artist"),
        ] {
            lectures.push((item_type, lire_les_favoris(&**svc, fav_type).await));
        }
        (service, lectures)
    };
    let profils = profils(backend, appelant);
    for (item_type, lecture) in lectures {
        match lecture {
            Ok(entrees) => {
                if let Err(e) =
                    reconcilier(backend, &profils, &service, item_type, entrees, &mut bilan)
                {
                    warn!(service = %service, item_type, erreur = %e, "favoris_miroir_base_refuse");
                    bilan.echecs += 1;
                    bilan.erreur = Some(e);
                }
            }
            Err(e) => {
                // Lecture ratée : RIEN n'est retiré pour ce type.
                warn!(service = %service, item_type, erreur = %e, "favoris_miroir_lecture_impossible");
                bilan.echecs += 1;
                bilan.erreur = Some(e);
            }
        }
    }
    if let Err(e) = pousser_en_attente(arc, backend, &service, &mut bilan).await {
        bilan.erreur = Some(e);
    }
    bilan.en_attente = compter_en_attente(backend, &service);
    if bilan.ajoutes + bilan.retires + bilan.pousses + bilan.echecs > 0 {
        info!(
            service = %service,
            lus = bilan.lus,
            ajoutes = bilan.ajoutes,
            retires = bilan.retires,
            pousses = bilan.pousses,
            en_attente = bilan.en_attente,
            echecs = bilan.echecs,
            "favoris_miroir_rafraichis"
        );
    }
    noter_rafraichissement(&service, &bilan);
    bilan
}

/// Rafraîchit si le miroir est périmé (ou si `forcer`). Un seul
/// rafraîchissement à la fois par service ; le TTL est relu une fois le
/// verrou pris, pour que dix lectures simultanées ne fassent qu'un passage.
pub async fn rafraichir_si_perime(
    arc: &ServiceArc,
    backend: &Arc<dyn DbBackend>,
    appelant: i64,
    forcer: bool,
) -> Option<BilanMiroir> {
    let service = arc.read().await.name().to_string();
    let verrou = verrou(&service);
    let _garde = verrou.lock().await;
    if !forcer && !est_perime(&service, ttl()) {
        return None;
    }
    Some(rafraichir(arc, backend, appelant).await)
}

#[cfg(test)]
#[path = "favorites_mirror_tests.rs"]
mod tests;
