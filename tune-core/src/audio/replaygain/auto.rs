//! #8968 (PR 4) — le mode ReplayGain **Auto** : gain d'ALBUM quand la piste
//! jouée a, dans la file, une voisine immédiate (précédente ou suivante) du
//! même album ; gain de PISTE sinon. C'est le comportement de foobar2000 et
//! d'Audirvana.
//!
//! # Pourquoi
//!
//! En mode `Track`, un album lu dans l'ordre change de niveau à chaque
//! frontière, même sans blanc : les écarts voulus entre ses pistes (un
//! adagio après un tutti) sont effacés. En mode `Album`, une liste mêlée
//! d'albums différents garde les écarts d'un disque à l'autre. `Auto` prend
//! le bon des deux selon ce qui se joue.
//!
//! # Les règles
//!
//! * **Même album = même identifiant d'album**, jamais un titre ressemblant :
//!   - deux lignes locales : le même `tracks.album_id`. Un album regroupé en
//!     versions (#7617, l'« œuvre » : l'édition 2011 et la Deluxe) reste
//!     plusieurs albums ici : chaque version a son `album_id` et son propre
//!     gain d'album, et c'est la version JOUÉE qui compte. Mêler deux versions
//!     sous un gain d'album serait appliquer à l'une le gain mesuré sur
//!     l'autre ;
//!   - deux titres d'un service : la même source et la même référence
//!     d'album du service (`album_ref`) ;
//!   - tout le reste (local contre service, identifiant absent) : PAS le même
//!     album. Dans le doute, gain de piste — le comportement d'avant.
//! * **Une voisine suffit**, avant ou après : la première et la dernière
//!   piste d'un album lu dans l'ordre reçoivent donc aussi le gain d'album,
//!   et aucune frontière intérieure ne saute de niveau.
//! * **Aléatoire : gain de piste**, toujours. Deux voisines tirées au hasard
//!   dans le même album ne font pas un album écouté ; un niveau qui changerait
//!   de règle au gré du tirage serait incompréhensible.
//! * **Une piste qu'on ne retrouve pas dans la file** (version substituée par
//!   la règle de version #2264, lecture hors file) : gain de piste.
//! * **Décidé une fois par lancement.** La granularité est décidée quand la
//!   piste est lancée (ou armée pour l'enchaînement), puis RETENUE pour la
//!   zone et relue partout : sortie locale, flux réseau, empreinte du
//!   traitement, chemin du signal, réglage du préampli. Un « Lire ensuite »
//!   ou un retrait dans la file ne change jamais le gain d'une piste en
//!   cours ; la file n'est relue qu'au lancement suivant.
//! * **Une requête bornée** par décision : la ligne en cours et ses voisines
//!   (`PlayQueueRepo::voisinage_du_courant`), jamais la file entière.
//! * La suivante de la dernière ligne n'est la première que si la zone
//!   répète toute la file.
//! * Le mode n'est résolu qu'ICI : en aval, `Auto` n'atteint jamais le calcul
//!   du gain (un `Auto` non résolu vaut `Track` partout, par prudence).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use tracing::debug;

use super::{PisteRg, ReplayGainMode};
use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueEntry};

/// Ce que la décision doit savoir de la zone et que la base ne porte pas :
/// l'aléatoire et la répétition de toute la file. Tenu à jour par
/// `PlaybackManager::set_shuffle` / `set_repeat` (les seuls écrivains de
/// `ZoneState::shuffle` / `repeat`), et copié avec la file d'une zone à
/// l'autre ([`copier_l_etat_de_zone`], transfert et groupe #6155).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EtatDeZone {
    aleatoire: bool,
    boucle: bool,
}

static ETATS: LazyLock<Mutex<HashMap<i64, EtatDeZone>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn etat(zone_id: i64) -> EtatDeZone {
    ETATS
        .lock()
        .ok()
        .and_then(|e| e.get(&zone_id).copied())
        .unwrap_or_default()
}

fn modifier(zone_id: i64, f: impl FnOnce(&mut EtatDeZone)) {
    if let Ok(mut e) = ETATS.lock() {
        f(e.entry(zone_id).or_default());
    }
}

/// Retenir l'état de l'aléatoire d'une zone.
pub fn noter_aleatoire(zone_id: i64, actif: bool) {
    modifier(zone_id, |e| e.aleatoire = actif);
}

/// Retenir si la zone répète toute la file (la suivante de la dernière
/// ligne est alors la première).
pub fn noter_boucle(zone_id: i64, toute_la_file: bool) {
    modifier(zone_id, |e| e.boucle = toute_la_file);
}

/// L'aléatoire est-il armé sur la zone ?
pub fn aleatoire(zone_id: i64) -> bool {
    etat(zone_id).aleatoire
}

/// La file d'une zone recopiée sur une autre (transfert, suiveuse d'un
/// groupe) : l'aléatoire et la répétition suivent, sinon la suiveuse
/// jugerait « même album » deux voisines tirées au hasard.
pub fn copier_l_etat_de_zone(de: i64, vers: i64) {
    let source = etat(de);
    modifier(vers, |e| *e = source);
}

/// Une piste, telle que la mémoire des décisions la reconnaît.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClePiste {
    Bibliotheque(i64),
    Service(String, String),
}

fn cle(piste: PisteRg<'_>) -> Option<ClePiste> {
    match (piste.track_id, piste.service) {
        (Some(t), _) => Some(ClePiste::Bibliotheque(t)),
        (None, Some((s, id))) => Some(ClePiste::Service(
            s.trim().to_ascii_lowercase(),
            id.trim().to_string(),
        )),
        (None, None) => None,
    }
}

/// Combien de décisions une zone retient : la piste qui joue et celle
/// qu'on arme pour l'enchaînement.
const DECISIONS_PAR_ZONE: usize = 2;

/// Les décisions prises, par zone : la granularité est décidée UNE fois, au
/// lancement de la piste (ou quand on l'arme pour l'enchaînement), puis
/// relue telle quelle — un « Lire ensuite », un retrait dans la file, un
/// préampli touché ne changent jamais le gain d'une piste en cours.
static DECISIONS: LazyLock<Mutex<HashMap<i64, VecDeque<(ClePiste, ReplayGainMode)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn decision_retenue(zone_id: i64, cle: &ClePiste) -> Option<ReplayGainMode> {
    DECISIONS
        .lock()
        .ok()?
        .get(&zone_id)?
        .iter()
        .find(|(c, _)| c == cle)
        .map(|(_, m)| *m)
}

fn retenir(zone_id: i64, cle: ClePiste, mode: ReplayGainMode) {
    let Ok(mut d) = DECISIONS.lock() else { return };
    let file = d.entry(zone_id).or_default();
    file.retain(|(c, _)| *c != cle);
    file.push_back((cle, mode));
    while file.len() > DECISIONS_PAR_ZONE {
        file.pop_front();
    }
}

/// Oublier les décisions d'une zone (témoins, zone supprimée).
pub fn oublier_les_decisions(zone_id: i64) {
    if let Ok(mut d) = DECISIONS.lock() {
        d.remove(&zone_id);
    }
}

/// Ce qu'une ligne de file dit de son album, et de quoi la reconnaître.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LigneDeFile {
    pub track_id: Option<i64>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    /// `tracks.album_id` d'une ligne locale.
    pub album_id: Option<i64>,
    /// La référence d'album du service, pour une ligne de service.
    pub album_ref: Option<String>,
}

impl From<&QueueEntry> for LigneDeFile {
    fn from(e: &QueueEntry) -> Self {
        Self {
            track_id: e.track_id,
            source: e.source.clone(),
            source_id: e.source_id.clone(),
            album_id: e.album_id,
            album_ref: e.album_id_service().map(str::to_string),
        }
    }
}

fn non_vide(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn meme_source(a: Option<&str>, b: Option<&str>) -> bool {
    match (non_vide(a), non_vide(b)) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// Deux lignes du MÊME album, par identifiant seulement (voir le module).
#[must_use]
pub fn meme_album(a: &LigneDeFile, b: &LigneDeFile) -> bool {
    if a.track_id.is_some() && b.track_id.is_some() {
        return matches!((a.album_id, b.album_id), (Some(x), Some(y)) if x == y);
    }
    if a.track_id.is_none() && b.track_id.is_none() {
        return meme_source(a.source.as_deref(), b.source.as_deref())
            && matches!(
                (non_vide(a.album_ref.as_deref()), non_vide(b.album_ref.as_deref())),
                (Some(x), Some(y)) if x == y
            );
    }
    false
}

/// La ligne est-elle la piste dont on calcule le gain ?
#[must_use]
pub fn est_la_piste(ligne: &LigneDeFile, piste: PisteRg<'_>) -> bool {
    match (piste.track_id, piste.service) {
        (Some(tid), _) => ligne.track_id == Some(tid),
        (None, Some((source, id))) => {
            ligne.track_id.is_none()
                && meme_source(ligne.source.as_deref(), Some(source))
                && non_vide(ligne.source_id.as_deref()) == non_vide(Some(id))
        }
        (None, None) => false,
    }
}

/// Pourquoi `Auto` a choisi ce qu'il a choisi (journal et témoins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotifAuto {
    Aleatoire,
    HorsDeLaFile,
    VoisineDuMemeAlbum,
    AucuneVoisineDuMemeAlbum,
}

impl MotifAuto {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aleatoire => "aleatoire",
            Self::HorsDeLaFile => "hors_de_la_file",
            Self::VoisineDuMemeAlbum => "voisine_du_meme_album",
            Self::AucuneVoisineDuMemeAlbum => "aucune_voisine_du_meme_album",
        }
    }
}

/// **La décision, fonction pure.**
///
/// `courant` est la position de la ligne en cours, `ligne_a` rend une ligne
/// par sa position (`None` hors de la file ou hors de la fenêtre lue). La
/// piste est cherchée à la position en cours (elle démarre), puis à la
/// suivante (elle est armée pour l'enchaînement), puis — seulement quand la
/// zone répète toute la file (`boucle`) et que la ligne en cours est la
/// dernière — à la position 0.
pub fn decider(
    piste: PisteRg<'_>,
    aleatoire: bool,
    boucle: bool,
    courant: Option<i64>,
    ligne_a: impl Fn(i64) -> Option<LigneDeFile>,
) -> (ReplayGainMode, MotifAuto) {
    if aleatoire {
        return (ReplayGainMode::Track, MotifAuto::Aleatoire);
    }
    let Some(c) = courant else {
        return (ReplayGainMode::Track, MotifAuto::HorsDeLaFile);
    };
    let suivant = if ligne_a(c + 1).is_some() {
        Some(c + 1)
    } else if boucle && c > 0 {
        Some(0)
    } else {
        None
    };
    let trouve = std::iter::once(c).chain(suivant).find_map(|pos| {
        ligne_a(pos)
            .filter(|l| est_la_piste(l, piste))
            .map(|l| (pos, l))
    });
    let Some((pos, ligne)) = trouve else {
        return (ReplayGainMode::Track, MotifAuto::HorsDeLaFile);
    };
    let voisine_du_meme_album = [pos - 1, pos + 1]
        .into_iter()
        .filter_map(&ligne_a)
        .any(|v| meme_album(&ligne, &v));
    if voisine_du_meme_album {
        (ReplayGainMode::Album, MotifAuto::VoisineDuMemeAlbum)
    } else {
        (ReplayGainMode::Track, MotifAuto::AucuneVoisineDuMemeAlbum)
    }
}

/// La granularité EFFECTIVE pour une piste d'une zone : `mode` tel quel hors
/// `Auto`. En `Auto` : la décision RETENUE pour cette piste sur cette zone si
/// elle existe ; sinon une décision neuve, prise sur la file de la zone
/// ([`decider`], une requête bornée) et retenue. Sans zone, ou si la base ne
/// répond pas : gain de piste.
pub fn granularite(
    backend: &Arc<dyn DbBackend>,
    zone_id: Option<i64>,
    piste: PisteRg<'_>,
    mode: ReplayGainMode,
) -> ReplayGainMode {
    if mode != ReplayGainMode::Auto {
        return mode;
    }
    let (Some(zone_id), Some(cle)) = (zone_id, cle(piste)) else {
        return ReplayGainMode::Track;
    };
    if let Some(retenue) = decision_retenue(zone_id, &cle) {
        return retenue;
    }
    let (choix, motif) = decider_sur_la_file(backend, zone_id, piste);
    debug!(
        zone_id,
        granularite = if choix == ReplayGainMode::Album {
            "album"
        } else {
            "track"
        },
        motif = motif.as_str(),
        "replaygain_auto"
    );
    retenir(zone_id, cle, choix);
    choix
}

fn decider_sur_la_file(
    backend: &Arc<dyn DbBackend>,
    zone_id: i64,
    piste: PisteRg<'_>,
) -> (ReplayGainMode, MotifAuto) {
    let EtatDeZone { aleatoire, boucle } = etat(zone_id);
    if aleatoire {
        return (ReplayGainMode::Track, MotifAuto::Aleatoire);
    }
    let Ok(lignes) =
        PlayQueueRepo::with_backend(backend.clone()).voisinage_du_courant(zone_id, boucle)
    else {
        return (ReplayGainMode::Track, MotifAuto::HorsDeLaFile);
    };
    // Une file abîmée peut porter deux lignes à la même position : la
    // première dans l'ordre `(position, id)`, comme `get_at`.
    let mut par_position: HashMap<i64, LigneDeFile> = HashMap::new();
    let mut courant = None;
    for e in &lignes {
        if e.is_current && courant.is_none() {
            courant = Some(e.position);
        }
        par_position
            .entry(e.position)
            .or_insert_with(|| LigneDeFile::from(e));
    }
    decider(piste, false, boucle, courant, |p| {
        par_position.get(&p).cloned()
    })
}

#[cfg(test)]
#[path = "auto_tests.rs"]
mod tests;
