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
//! * Le mode n'est résolu qu'ICI : en aval, `Auto` n'atteint jamais le calcul
//!   du gain (un `Auto` non résolu vaut `Track` partout, par prudence).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use tracing::debug;

use super::{PisteRg, ReplayGainMode};
use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueEntry};

/// Les zones dont l'aléatoire est armé, tenu à jour par
/// `PlaybackManager::set_shuffle` — le seul écrivain de `ZoneState::shuffle`.
/// Le calcul du gain ne voit que la base et la piste : c'est ce registre qui
/// lui dit l'état de l'aléatoire sans lui passer l'état de zone.
static ALEATOIRE: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Retenir l'état de l'aléatoire d'une zone.
pub fn noter_aleatoire(zone_id: i64, actif: bool) {
    let Ok(mut zones) = ALEATOIRE.lock() else {
        return;
    };
    if actif {
        zones.insert(zone_id);
    } else {
        zones.remove(&zone_id);
    }
}

/// L'aléatoire est-il armé sur la zone ?
pub fn aleatoire(zone_id: i64) -> bool {
    ALEATOIRE.lock().is_ok_and(|zones| zones.contains(&zone_id))
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
/// `courant` est le rang de la ligne en cours, `longueur` celle de la file,
/// `ligne_au_rang` rend une ligne par son rang. La piste est cherchée au rang
/// en cours (elle démarre), puis au rang suivant (elle est armée pour
/// l'enchaînement), puis au rang 0 quand la file reboucle (répétition de
/// toute la file).
pub fn decider(
    piste: PisteRg<'_>,
    aleatoire: bool,
    courant: Option<i64>,
    longueur: i64,
    ligne_au_rang: impl Fn(i64) -> Option<LigneDeFile>,
) -> (ReplayGainMode, MotifAuto) {
    if aleatoire {
        return (ReplayGainMode::Track, MotifAuto::Aleatoire);
    }
    let Some(c) = courant else {
        return (ReplayGainMode::Track, MotifAuto::HorsDeLaFile);
    };
    let suivant = if c + 1 < longueur { c + 1 } else { 0 };
    let trouve = [c, suivant].into_iter().find_map(|rang| {
        ligne_au_rang(rang)
            .filter(|l| est_la_piste(l, piste))
            .map(|l| (rang, l))
    });
    let Some((rang, ligne)) = trouve else {
        return (ReplayGainMode::Track, MotifAuto::HorsDeLaFile);
    };
    let voisine_du_meme_album = [rang - 1, rang + 1]
        .into_iter()
        .filter(|r| (0..longueur).contains(r))
        .filter_map(&ligne_au_rang)
        .any(|v| meme_album(&ligne, &v));
    if voisine_du_meme_album {
        (ReplayGainMode::Album, MotifAuto::VoisineDuMemeAlbum)
    } else {
        (ReplayGainMode::Track, MotifAuto::AucuneVoisineDuMemeAlbum)
    }
}

/// La granularité EFFECTIVE pour une piste d'une zone : `mode` tel quel hors
/// `Auto` ; sinon la décision de [`decider`] sur la file de la zone. Sans
/// zone, ou si la base ne répond pas : gain de piste.
pub fn granularite(
    backend: &Arc<dyn DbBackend>,
    zone_id: Option<i64>,
    piste: PisteRg<'_>,
    mode: ReplayGainMode,
) -> ReplayGainMode {
    if mode != ReplayGainMode::Auto {
        return mode;
    }
    let Some(zone_id) = zone_id else {
        return ReplayGainMode::Track;
    };
    let aleatoire = aleatoire(zone_id);
    if aleatoire {
        return ReplayGainMode::Track;
    }
    let repo = PlayQueueRepo::with_backend(backend.clone());
    let Ok((courant, longueur)) = repo.current_index_and_length(zone_id) else {
        return ReplayGainMode::Track;
    };
    // Quatre lignes autour du rang en cours (précédente, en cours, suivante,
    // celle d'après), plus les deux premières quand la file reboucle : de quoi
    // juger la piste qui démarre ET celle qu'on arme, sans relire la file.
    let mut lignes: HashMap<i64, LigneDeFile> = HashMap::new();
    let mut lire = |debut: i64, nombre: i64| {
        if let Ok(rangs) = repo.get_ordered_window(zone_id, debut, nombre) {
            for (i, e) in rangs.iter().enumerate() {
                lignes.insert(debut + i as i64, LigneDeFile::from(e));
            }
        }
    };
    if let Some(c) = courant {
        let debut = (c - 1).max(0);
        lire(debut, c + 3 - debut);
        if c + 1 >= longueur {
            lire(0, 2);
        }
    }
    let (choix, motif) = decider(piste, false, courant, longueur, |r| lignes.get(&r).cloned());
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
    choix
}

#[cfg(test)]
#[path = "auto_tests.rs"]
mod tests;
