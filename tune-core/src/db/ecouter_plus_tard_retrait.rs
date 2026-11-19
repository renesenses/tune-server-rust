//! #9143 (fil forum 2934, validé par Bertrand le 19/11/2026) — « Écouter
//! plus tard » rend un album écouté EN ENTIER, si le profil l'a demandé.
//!
//!     « Retirer automatiquement après lecture ? Oui ou Non »
//!
//! Le sas est une étiquette ordinaire (web#1653) dont l'identifiant vit dans
//! les réglages du profil, sous [`CLE_ETIQUETTE`]. Le retrait automatique est
//! une seconde clé du même objet, [`CLE_RETRAIT_AUTO`], **absente = non** :
//! le sas a été pensé pour « voir ce qu'on en fait » après l'écoute (FabienM,
//! fil 1986) ; retirer d'office changerait en silence l'usage de ceux qui s'en
//! servent ainsi.
//!
//! ## L'écoute complète, telle qu'elle se teste
//!
//! Dans UNE zone, pendant une session « album » (`session_context_type =
//! album`, posée par « Lire » sur l'album) et pour le profil de la session :
//!
//! - une piste est ACHEVÉE quand sa position atteint [`SEUIL_POURCENT`] % de
//!   sa durée (relevé du sondeur), ou à sa fin naturelle (`handle_track_end`) ;
//! - les pistes comptées sont les lignes de file de l'album (`tracks.album_id`
//!   pour la bibliothèque, service + `album_ref` pour un service) ;
//! - l'écoute est COMPLÈTE quand TOUTES les pistes DE L'ALBUM ont été
//!   achevées dans la session, pas seulement celles de la file : pour la
//!   bibliothèque, toutes ses pistes en base ; pour un service, autant de
//!   pistes distinctes que l'album en compte chez lui (`track_count`). « Lire
//!   le disque 2 » ou une file retouchée ne vident donc pas le sas. Un total
//!   qui ne s'établit pas (service muet, déconnecté) : rien n'est retiré.
//!
//! Une autre session (autre album, file réécrite : la ligne jouée n'est plus
//! du décompte) repart de zéro. Une session sans profil ne touche à rien.
//! Une écoute complète est traitée UNE fois : l'album est ensuite « déjà
//! traité » pour la session (single, piste en répétition).
//!
//! ## Le coût dans le sondeur
//!
//! Appelé à chaque tour pour chaque zone en lecture. Dans l'ordre :
//! les tests gratuits (90 %, session « album », profil), puis la clé de piste
//! mémorisée (une piste n'est examinée qu'UNE fois, quelle que soit l'issue),
//! puis l'option du profil, gardée en cache ([`SasRetrait::reglages_modifies`] l'invalide,
//! et elle se relit au plus toutes les [`DUREE_CACHE_OPTION`]). **Option
//! éteinte : aucune lecture de la file, une seule lecture des réglages.** Le
//! verrou n'est jamais tenu pendant une lecture en base.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::play_queue_repo::{PlayQueueRepo, QueueEntry};
use super::tag_repo::TagRepo;
use crate::library::regle_de_version::reglages_du_profil;
use crate::playback::ZoneState;

/// La clé de profil qui retient l'étiquette du sas (écrite par le client web,
/// `lib/ecouterPlusTard.ts`).
pub const CLE_ETIQUETTE: &str = "ecouterPlusTardEtiquette";

/// La clé de profil de l'option : `true` = retirer après écoute complète.
pub const CLE_RETRAIT_AUTO: &str = "ecouterPlusTardRetraitAuto";

/// Une piste est achevée à partir de ce pourcentage de sa durée.
pub const SEUIL_POURCENT: i64 = 90;

/// L'option d'un profil se relit au plus tard au bout de cette durée, même
/// sans [`SasRetrait::reglages_modifies`] (réglages restaurés d'une
/// sauvegarde…).
pub const DUREE_CACHE_OPTION: Duration = Duration::from_secs(300);

/// La piste arrivée à `position_ms` sur `duree_ms` est-elle achevée ?
/// Jamais pour une durée inconnue : seule la fin naturelle le dira.
pub fn piste_achevee(position_ms: i64, duree_ms: i64) -> bool {
    duree_ms > 0
        && position_ms > 0
        && position_ms.saturating_mul(100) >= duree_ms.saturating_mul(SEUIL_POURCENT)
}

/// L'album d'une session d'écoute, dans son espace de noms.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AlbumEcoute {
    /// Un album de la bibliothèque (`albums.id`).
    Local(i64),
    /// Un album de service : `("qobuz", "0060254735822")`, source en
    /// minuscules.
    Service { source: String, id: String },
}

impl AlbumEcoute {
    /// L'album que la zone écoute, si sa session est une session « album ».
    pub fn de_la_session(etat: &ZoneState) -> Option<Self> {
        if etat.session_context_type.as_deref() != Some("album") {
            return None;
        }
        let id = etat.session_context_id.as_deref()?.trim();
        if id.is_empty() {
            return None;
        }
        match etat
            .session_context_source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None | Some("local") => id.parse().ok().filter(|n| *n > 0).map(Self::Local),
            Some(source) => Some(Self::Service {
                source: source.to_lowercase(),
                id: id.to_string(),
            }),
        }
    }

    /// La ligne de file appartient-elle à cet album ?
    fn contient(&self, e: &QueueEntry) -> bool {
        match self {
            Self::Local(a) => e.is_local() && e.album_id == Some(*a),
            Self::Service { source, id } => {
                e.source
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(source))
                    && e.album_id_service() == Some(id.as_str())
            }
        }
    }
}

/// La clé d'une piste jouée : rang dans la file, titre, identifiant.
type ClePiste = (i64, String, Option<String>);

/// Le décompte d'une zone.
#[derive(Debug, Clone, Default)]
struct SuiviZone {
    /// La dernière piste examinée : elle ne l'est plus aux tours suivants.
    derniere: Option<ClePiste>,
    album: Option<AlbumEcoute>,
    /// Les lignes de file de l'album au début du décompte : une ligne jouée
    /// qui n'en est pas signe une file réécrite, donc une autre session.
    lignes: BTreeSet<i64>,
    lignes_achevees: BTreeSet<i64>,
    /// Les pistes distinctes achevées : `tracks.id`, ou l'identifiant chez le
    /// service.
    pistes_locales: BTreeSet<i64>,
    pistes_service: BTreeSet<String>,
    /// L'écoute complète de cette session est déjà traitée.
    traite: bool,
}

/// L'option d'un profil en cache : l'étiquette du sas si l'option est
/// allumée, `None` sinon.
#[derive(Debug, Clone, Copy)]
struct OptionLue {
    generation: u64,
    lue_a: Instant,
    sas: Option<i64>,
}

/// Ce qu'une écoute complète a produit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcouteComplete {
    pub album: AlbumEcoute,
    /// L'album est sorti du sas (il y était).
    pub retire: bool,
}

/// Un album de service dont toutes les lignes de file sont achevées : reste
/// à savoir combien de pistes il compte chez son service (appel réseau, hors
/// du sondeur), puis [`SasRetrait::conclure_service`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationService {
    pub zone_id: i64,
    pub source: String,
    pub id: String,
    pub sas: i64,
    pub pistes_achevees: usize,
}

/// L'issue de [`SasRetrait::noter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Suite {
    /// Album de la bibliothèque écouté en entier.
    Complete(EcouteComplete),
    /// Album de service : le total reste à établir.
    VerifierService(VerificationService),
}

#[derive(Debug, Default)]
struct Etat {
    suivis: HashMap<i64, SuiviZone>,
    options: HashMap<i64, OptionLue>,
}

/// Le retrait automatique : décomptes par zone et option par profil.
#[derive(Debug, Default)]
pub struct SasRetrait {
    etat: Mutex<Etat>,
    /// Bumpée à chaque écriture des réglages d'un profil : l'option en
    /// cache se relit alors.
    generation: AtomicU64,
    /// Lectures en base faites par ce module (épreuve du coût).
    lectures: AtomicUsize,
}

impl SasRetrait {
    pub fn new() -> Self {
        Self::default()
    }

    /// Un verrou empoisonné (panique d'un autre appel) se reprend : les
    /// décomptes ne sont qu'une mémoire, rien d'incohérent à protéger.
    fn verrou(&self) -> MutexGuard<'_, Etat> {
        self.etat.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// À appeler après toute écriture des réglages d'un profil
    /// (`POST /profiles/{id}/settings`).
    pub fn reglages_modifies(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Combien de lectures en base ce module a faites.
    pub fn lectures(&self) -> usize {
        self.lectures.load(Ordering::Relaxed)
    }

    fn compter(&self) {
        self.lectures.fetch_add(1, Ordering::Relaxed);
    }

    /// Note la piste que joue `etat` si elle est achevée (`naturel` = fin
    /// naturelle). Rend la suite quand c'était la dernière qui manquait.
    pub fn noter(
        &self,
        db: &Arc<dyn DbBackend>,
        etat: &ZoneState,
        naturel: bool,
    ) -> Result<Option<Suite>, String> {
        // 1. Les tests gratuits.
        let Some(np) = etat.now_playing.as_ref() else {
            return Ok(None);
        };
        if !naturel && !piste_achevee(etat.position_ms, np.duration_ms) {
            return Ok(None);
        }
        let Some(album) = AlbumEcoute::de_la_session(etat) else {
            return Ok(None);
        };
        let Some(profil) = etat.session_profile_id else {
            return Ok(None);
        };

        // 2. Une piste n'est examinée qu'une fois, quelle que soit l'issue.
        let cle: ClePiste = (etat.queue_position, np.title.clone(), np.source_id.clone());
        {
            let mut e = self.verrou();
            let suivi = e.suivis.entry(etat.zone_id).or_default();
            if suivi.derniere.as_ref() == Some(&cle) {
                return Ok(None);
            }
            suivi.derniere = Some(cle);
        }

        // 3. L'option du profil, en cache.
        let Some(sas) = self.option(db, profil) else {
            return Ok(None);
        };

        // 4. La ligne jouée, lue hors du verrou.
        self.compter();
        let file = PlayQueueRepo::with_backend(db.clone()).get_ordered(etat.zone_id)?;
        let Some(courante) = file
            .iter()
            .find(|e| e.position == etat.queue_position)
            .filter(|e| est_la_piste_jouee(e, etat))
        else {
            return Ok(None);
        };
        if !album.contient(courante) {
            return Ok(None);
        }

        // 5. Le décompte.
        let (pistes_locales, pistes_service) = {
            let mut e = self.verrou();
            let suivi = e.suivis.entry(etat.zone_id).or_default();
            if suivi.album.as_ref() != Some(&album) || !suivi.lignes.contains(&courante.id) {
                let derniere = suivi.derniere.take();
                *suivi = SuiviZone {
                    derniere,
                    album: Some(album.clone()),
                    lignes: file
                        .iter()
                        .filter(|e| album.contient(e))
                        .map(|e| e.id)
                        .collect(),
                    ..Default::default()
                };
            }
            if suivi.traite {
                return Ok(None);
            }
            suivi.lignes_achevees.insert(courante.id);
            if let Some(t) = courante.track_id {
                suivi.pistes_locales.insert(t);
            }
            if let Some(sid) = courante.source_id.as_deref().filter(|s| !s.is_empty()) {
                suivi.pistes_service.insert(sid.to_string());
            }
            if !suivi.lignes.is_subset(&suivi.lignes_achevees) {
                return Ok(None);
            }
            (suivi.pistes_locales.clone(), suivi.pistes_service.len())
        };

        match album {
            AlbumEcoute::Local(a) => {
                self.compter();
                if !pistes_de_l_album(db, a)?.is_subset(&pistes_locales) {
                    return Ok(None);
                }
                self.marquer_traite(etat.zone_id, &album);
                let retire = retirer_du_sas(db, sas, &album)?;
                tracing::info!(
                    zone_id = etat.zone_id,
                    album = ?album,
                    retire,
                    "ecouter_plus_tard_ecoute_complete"
                );
                Ok(Some(Suite::Complete(EcouteComplete { album, retire })))
            }
            AlbumEcoute::Service { source, id } => {
                Ok(Some(Suite::VerifierService(VerificationService {
                    zone_id: etat.zone_id,
                    source,
                    id,
                    sas,
                    pistes_achevees: pistes_service,
                })))
            }
        }
    }

    /// Conclut un album de service une fois son total connu (`None` : le
    /// service ne l'a pas dit — rien n'est retiré).
    pub fn conclure_service(
        &self,
        db: &Arc<dyn DbBackend>,
        v: &VerificationService,
        total: Option<u32>,
    ) -> Result<Option<EcouteComplete>, String> {
        let Some(total) = total.filter(|t| *t > 0) else {
            tracing::info!(
                zone_id = v.zone_id,
                source = %v.source,
                id = %v.id,
                "ecouter_plus_tard_total_inconnu"
            );
            return Ok(None);
        };
        if v.pistes_achevees < total as usize {
            return Ok(None);
        }
        let album = AlbumEcoute::Service {
            source: v.source.clone(),
            id: v.id.clone(),
        };
        if !self.marquer_traite(v.zone_id, &album) {
            return Ok(None);
        }
        let retire = retirer_du_sas(db, v.sas, &album)?;
        tracing::info!(
            zone_id = v.zone_id,
            album = ?album,
            retire,
            "ecouter_plus_tard_ecoute_complete"
        );
        Ok(Some(EcouteComplete { album, retire }))
    }

    /// Marque l'écoute complète traitée, si la zone est toujours sur cet
    /// album et qu'elle ne l'était pas déjà. Rend `true` quand elle vient de
    /// l'être.
    fn marquer_traite(&self, zone_id: i64, album: &AlbumEcoute) -> bool {
        let mut e = self.verrou();
        match e.suivis.get_mut(&zone_id) {
            Some(s) if s.album.as_ref() == Some(album) && !s.traite => {
                s.traite = true;
                true
            }
            _ => false,
        }
    }

    /// L'étiquette du sas du profil si l'option y est allumée, en cache.
    fn option(&self, db: &Arc<dyn DbBackend>, profil: i64) -> Option<i64> {
        let generation = self.generation.load(Ordering::Relaxed);
        if let Some(o) = self.verrou().options.get(&profil)
            && o.generation == generation
            && o.lue_a.elapsed() < DUREE_CACHE_OPTION
        {
            return o.sas;
        }
        self.compter();
        let reglages = reglages_du_profil(db, profil);
        let sas = (reglages.get(CLE_RETRAIT_AUTO).and_then(Value::as_bool) == Some(true))
            .then(|| identifiant(reglages.get(CLE_ETIQUETTE)))
            .flatten();
        self.verrou().options.insert(
            profil,
            OptionLue {
                generation,
                lue_a: Instant::now(),
                sas,
            },
        );
        sas
    }
}

/// La ligne de file désigne-t-elle bien la piste en lecture ?
fn est_la_piste_jouee(e: &QueueEntry, etat: &ZoneState) -> bool {
    let Some(np) = etat.now_playing.as_ref() else {
        return false;
    };
    match (e.track_id, np.track_id) {
        (Some(a), Some(b)) => a == b,
        (None, None) => np.source_id.is_some() && e.source_id == np.source_id,
        _ => false,
    }
}

fn ph(engine: Engine, i: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    }
}

/// Les pistes de l'album `album_id` en bibliothèque.
fn pistes_de_l_album(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<BTreeSet<i64>, String> {
    Ok(db
        .query_many(
            &format!(
                "SELECT id FROM tracks WHERE album_id = {}",
                ph(db.engine(), 1)
            ),
            &[&album_id as &dyn ToSqlValue],
        )?
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect())
}

/// Un identifiant d'étiquette, écrit en nombre ou en texte.
fn identifiant(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|id| *id > 0)
}

/// Sort l'album de l'étiquette `sas`, et d'elle seule. Rend `true` quand une
/// ligne y était.
fn retirer_du_sas(db: &Arc<dyn DbBackend>, sas: i64, album: &AlbumEcoute) -> Result<bool, String> {
    let tags = TagRepo::with_backend(db.clone());
    match album {
        AlbumEcoute::Local(a) => {
            if !tags.items_by_tag(sas, "album")?.contains(a) {
                return Ok(false);
            }
            tags.untag_item(sas, "album", *a)?;
        }
        AlbumEcoute::Service { source, id } => {
            // La source telle qu'elle est RANGÉE (« Qobuz » ou « qobuz ») :
            // le `DELETE` compare exactement.
            let Some(ligne) = tags
                .streaming_items_by_tag(sas, "album")?
                .into_iter()
                .find(|i| i.source.eq_ignore_ascii_case(source) && i.source_id == *id)
            else {
                return Ok(false);
            };
            tags.untag_streaming_item(sas, "album", &ligne.source, &ligne.source_id)?;
        }
    }
    Ok(true)
}

#[cfg(test)]
#[path = "ecouter_plus_tard_retrait_tests.rs"]
mod tests;
