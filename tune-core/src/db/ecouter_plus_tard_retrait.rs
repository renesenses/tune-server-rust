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
//! album`, posée par « Lire » sur l'album) :
//!
//! - les pistes de l'album sont les lignes de la file qui lui appartiennent :
//!   `tracks.album_id` pour la bibliothèque, le service et `album_ref` pour
//!   une piste de service ;
//! - une piste est ACHEVÉE quand sa position atteint [`SEUIL_POURCENT`] % de
//!   sa durée (relevé du sondeur), ou à sa fin naturelle (`handle_track_end`) ;
//! - l'écoute est COMPLÈTE quand toutes ces lignes sont achevées dans la même
//!   session. Pour un album de la bibliothèque, il faut en plus que TOUTES
//!   ses pistes en base l'aient été : un album lancé à la piste 5 n'a pas été
//!   écouté en entier.
//!
//! Une autre session (autre album, file réécrite : la ligne jouée n'est plus
//! du décompte) repart de zéro. Une écoute partielle ne retire donc rien.
//! Le décompte vit en mémoire : un redémarrage du serveur l'oublie, comme
//! l'écoute en attente de #7852.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde_json::Value;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::play_queue_repo::{PlayQueueRepo, QueueEntry};
use super::settings_repo::SettingsRepo;
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
    /// Un album de service : `("qobuz", "0060254735822")`.
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
                e.source.as_deref().map(str::to_lowercase).as_deref() == Some(source.as_str())
                    && e.album_id_service() == Some(id.as_str())
            }
        }
    }
}

/// Le décompte d'une zone : l'album, ses lignes de file, celles achevées.
#[derive(Debug, Clone)]
pub struct SuiviAlbum {
    album: AlbumEcoute,
    lignes: BTreeSet<i64>,
    achevees: BTreeSet<i64>,
    pistes_locales: BTreeSet<i64>,
    /// La dernière piste notée (position, titre) : le sondeur repasse à
    /// chaque tour au-delà du seuil, la base n'est relue qu'une fois.
    derniere: Option<(i64, String)>,
}

/// Les décomptes, par zone.
pub type Suivis = HashMap<i64, SuiviAlbum>;

/// Ce qu'une écoute complète a produit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcouteComplete {
    pub album: AlbumEcoute,
    /// L'album est sorti du sas (option active, sas connu, album dedans).
    pub retire: bool,
}

/// Note la piste que joue `etat` si elle est achevée (`naturel` = fin
/// naturelle), et rend l'écoute complète quand c'était la dernière qui
/// manquait. Le retrait du sas est fait ici, selon l'option du profil.
pub fn noter_piste_achevee(
    db: &Arc<dyn DbBackend>,
    suivis: &mut Suivis,
    etat: &ZoneState,
    naturel: bool,
) -> Result<Option<EcouteComplete>, String> {
    let Some(np) = etat.now_playing.as_ref() else {
        return Ok(None);
    };
    if !naturel && !piste_achevee(etat.position_ms, np.duration_ms) {
        return Ok(None);
    }
    let Some(album) = AlbumEcoute::de_la_session(etat) else {
        suivis.remove(&etat.zone_id);
        return Ok(None);
    };
    let cle = (etat.queue_position, np.title.clone());
    if suivis
        .get(&etat.zone_id)
        .is_some_and(|s| s.album == album && s.derniere.as_ref() == Some(&cle))
    {
        return Ok(None);
    }

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

    let suivi = suivis
        .entry(etat.zone_id)
        .or_insert_with(|| nouveau_suivi(&album, &file));
    if suivi.album != album || !suivi.lignes.contains(&courante.id) {
        *suivi = nouveau_suivi(&album, &file);
    }
    suivi.derniere = Some(cle);
    suivi.achevees.insert(courante.id);
    if let Some(t) = courante.track_id {
        suivi.pistes_locales.insert(t);
    }
    if !suivi.lignes.is_subset(&suivi.achevees) {
        return Ok(None);
    }
    if let AlbumEcoute::Local(a) = album
        && !pistes_de_l_album(db, a)?.is_subset(&suivi.pistes_locales)
    {
        return Ok(None);
    }
    suivis.remove(&etat.zone_id);

    let retire = retirer_du_sas(db, profil_de(db, etat), &album)?;
    tracing::info!(
        zone_id = etat.zone_id,
        album = ?album,
        retire,
        "ecouter_plus_tard_ecoute_complete"
    );
    Ok(Some(EcouteComplete { album, retire }))
}

fn nouveau_suivi(album: &AlbumEcoute, file: &[QueueEntry]) -> SuiviAlbum {
    SuiviAlbum {
        album: album.clone(),
        lignes: file
            .iter()
            .filter(|e| album.contient(e))
            .map(|e| e.id)
            .collect(),
        achevees: BTreeSet::new(),
        pistes_locales: BTreeSet::new(),
        derniere: None,
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

/// Le profil de la session, sinon le profil actif du serveur.
fn profil_de(db: &Arc<dyn DbBackend>, etat: &ZoneState) -> Option<i64> {
    etat.session_profile_id.or_else(|| {
        SettingsRepo::with_backend(db.clone())
            .get("active_profile_id")
            .ok()
            .flatten()
            .and_then(|t| t.trim().parse().ok())
    })
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

/// Sort l'album du sas du profil si l'option y est active. Rend `true` quand
/// une ligne d'étiquette a été retirée.
fn retirer_du_sas(
    db: &Arc<dyn DbBackend>,
    profile_id: Option<i64>,
    album: &AlbumEcoute,
) -> Result<bool, String> {
    let Some(profile_id) = profile_id else {
        return Ok(false);
    };
    let reglages = reglages_du_profil(db, profile_id);
    if reglages.get(CLE_RETRAIT_AUTO).and_then(Value::as_bool) != Some(true) {
        return Ok(false);
    }
    let Some(tag_id) = identifiant(reglages.get(CLE_ETIQUETTE)) else {
        return Ok(false);
    };
    let tags = TagRepo::with_backend(db.clone());
    match album {
        AlbumEcoute::Local(a) => {
            if !tags.items_by_tag(tag_id, "album")?.contains(a) {
                return Ok(false);
            }
            tags.untag_item(tag_id, "album", *a)?;
        }
        AlbumEcoute::Service { source, id } => {
            let present = tags
                .streaming_items_by_tag(tag_id, "album")?
                .iter()
                .any(|i| i.source.eq_ignore_ascii_case(source) && i.source_id == *id);
            if !present {
                return Ok(false);
            }
            tags.untag_streaming_item(tag_id, "album", source, id)?;
        }
    }
    Ok(true)
}

#[cfg(test)]
#[path = "ecouter_plus_tard_retrait_tests.rs"]
mod tests;
