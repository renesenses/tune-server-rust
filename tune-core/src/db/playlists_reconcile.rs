//! Réconciliation des lignes de playlist INTROUVABLES (#6179).
//!
//! `playlist_tracks.track_id` désigne un rowid de `tracks`, et ce rowid ne
//! survit pas à tous les scans : purge post-scan d'un chemin disparu puis
//! ré-insertion sous un nouvel id, surveillant (fichier modifié = supprimé puis
//! réinséré), « Vider la bibliothèque » puis rescan, fusion de doublons. Avec
//! `ON DELETE CASCADE`, chacun de ces chemins effaçait la ligne de playlist —
//! définitivement et sans trace (FabienM, fil 2236 : « ma playlist est revenue
//! vide »). Les favoris, eux, étaient réconciliés (`favorites_reconcile.rs`).
//!
//! Le mécanisme, en deux temps, comme pour les favoris :
//!
//! 1. **La ligne reste** (migrations SQLite 125 / PG 089). Un déclencheur
//!    `BEFORE DELETE ON tracks` (`migrations/playlist_tracks_orphelines.sql`)
//!    passe chaque ligne qui désigne la piste en ligne INTROUVABLE — ni
//!    `track_id` ni `source` — À SA POSITION, et y fige l'identité de la piste
//!    qui part : titre, artiste, album, durée, chemin, `audio_hash`,
//!    `audio_fingerprint`, MBID d'enregistrement. La clé étrangère est passée
//!    en `ON DELETE SET NULL` : même sans le déclencheur, plus aucune
//!    suppression de piste n'efface une ligne.
//! 2. **Elle se rattache** (ce module) quand la piste reparaît : au
//!    démarrage, après chaque scan, après chaque lot du surveillant et après
//!    une purge. La ligne reprend son `track_id`, garde sa position, et son
//!    instantané est vidé.
//!
//! Une ligne qui ne retrouve rien RESTE introuvable — jamais supprimée ici,
//! pas même après un scan complet : c'est la différence avec les favoris.
//! Une playlist est une liste que l'utilisateur a composée ; une ligne n'en
//! sort que par son geste (retirer de la playlist).
//!
//! ## Règles de rattachement, de la plus sûre à la plus faible
//!
//! 1. même chemin (`tracks.file_path`) — fichier revenu, rescan, retouche ;
//! 2. même `audio_hash` — fichier déplacé ou renommé, octets identiques ;
//! 3. même `audio_fingerprint` — contenu décodé identique (re-balisé) ;
//! 4. même MBID d'enregistrement — unique, ou unique sur le même album ;
//! 5. même titre et même artiste — unique, sinon unique sur le même album,
//!    sinon unique à ±2 s de durée.
//!
//! Plusieurs candidats aux règles 4 et 5 = AUCUN gagnant : une ligne qui
//! pointerait la mauvaise version (live au lieu de studio) est pire qu'une
//! ligne introuvable, qui se voit et se corrige. Aux règles 2 et 3, plusieurs
//! candidats sont des copies du même contenu : le plus petit id gagne.
//!
//! Les règles 1 à 4 sont des jointures indexées, une requête chacune quel que
//! soit le nombre de lignes introuvables. La règle 5 lit les titres en
//! mémoire, une fois par passe, et seulement pour ce qui reste — la passe
//! RAPIDE (surveillant) s'en dispense.
//!
//! SQL agnostique SQLite/PostgreSQL : placeholders `?`, ids en i64.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{info, warn};

use super::backend::{DbBackend, SqlValue, ToSqlValue};

/// La condition « ligne introuvable », alias `pt`.
const INTROUVABLE: &str = "pt.track_id IS NULL AND pt.source IS NULL AND pt.source_id IS NULL";

/// Tolérance de durée de la règle 5, en millisecondes.
const ECART_DE_DUREE_MS: i64 = 2_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlaylistsReconcileStats {
    /// Lignes introuvables examinées.
    pub scanned: u64,
    /// Lignes rattachées à la piste retrouvée, à leur position.
    pub relinked: u64,
    /// Lignes introuvables retirées parce que leur piste, retrouvée, était
    /// DÉJÀ dans la même playlist avant la passe (l'utilisateur l'y a remise).
    pub deduplicated: u64,
    /// Lignes locales qui désignaient une piste absente (base PostgreSQL
    /// basculée depuis SQLite, sans clé étrangère) passées en introuvables.
    pub detached: u64,
    /// Lignes qui restent introuvables.
    pub unresolved: u64,
}

impl PlaylistsReconcileStats {
    pub fn changed(&self) -> u64 {
        self.relinked + self.deduplicated + self.detached
    }
}

/// Une ligne introuvable et l'identité figée de sa piste.
#[derive(Debug, Clone)]
struct Orpheline {
    id: i64,
    playlist_id: i64,
    title: String,
    artist: String,
    album: String,
    duration_ms: Option<i64>,
}

pub struct PlaylistsReconciler {
    db: Arc<dyn DbBackend>,
}

impl PlaylistsReconciler {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    /// Passe complète, les cinq règles : démarrage, fin de scan, purge.
    pub fn run(&self) -> Result<PlaylistsReconcileStats, String> {
        self.passe(true)
    }

    /// Passe rapide, les quatre règles indexées seulement : après un lot du
    /// surveillant, qui tourne souvent. Un fichier retouché (même chemin) ou
    /// renommé (même contenu) y revient dans sa playlist sans attendre le
    /// prochain scan.
    pub fn run_rapide(&self) -> Result<PlaylistsReconcileStats, String> {
        self.passe(false)
    }

    fn passe(&self, complete: bool) -> Result<PlaylistsReconcileStats, String> {
        let mut stats = PlaylistsReconcileStats {
            detached: self.detacher_les_pendantes()?,
            ..Default::default()
        };

        let orphelines = self.orphelines()?;
        if orphelines.is_empty() {
            return Ok(stats);
        }
        stats.scanned = orphelines.len() as u64;

        // Règles 1 à 3 : un seul candidat retenu par ligne (le plus petit id).
        let mut cibles: HashMap<i64, i64> = HashMap::new();
        for colonne in ["file_path", "audio_hash", "audio_fingerprint"] {
            if cibles.len() == orphelines.len() {
                break;
            }
            for (ligne, piste) in self.par_jointure(colonne)? {
                cibles.entry(ligne).or_insert(piste);
            }
        }
        // Règle 4 : MBID.
        if cibles.len() < orphelines.len() {
            let par_album = self.candidats_mbid()?;
            for o in &orphelines {
                if cibles.contains_key(&o.id) {
                    continue;
                }
                if let Some(c) = par_album.get(&o.id)
                    && let Some(piste) = trancher(c, &o.album, None)
                {
                    cibles.insert(o.id, piste);
                }
            }
        }
        // Règle 5 : titre + artiste, en mémoire.
        if complete && cibles.len() < orphelines.len() {
            let restantes: Vec<&Orpheline> = orphelines
                .iter()
                .filter(|o| !cibles.contains_key(&o.id) && !o.title.trim().is_empty())
                .collect();
            if !restantes.is_empty() {
                let titres: HashSet<String> = restantes.iter().map(|o| plier(&o.title)).collect();
                let index = self.pistes_par_titre(&titres)?;
                for o in restantes {
                    let Some(memes_titres) = index.get(&plier(&o.title)) else {
                        continue;
                    };
                    let artiste = plier(&o.artist);
                    let candidats: Vec<Candidat> = memes_titres
                        .iter()
                        .filter(|c| c.artiste == artiste)
                        .cloned()
                        .collect();
                    if let Some(piste) = trancher(&candidats, &o.album, o.duration_ms) {
                        cibles.insert(o.id, piste);
                    }
                }
            }
        }

        // Écriture, dans l'ordre des lignes : une piste DÉJÀ dans la playlist
        // avant la passe absorbe la ligne introuvable ; une piste présente
        // deux fois (permis) revient deux fois.
        let mut deja: HashSet<(i64, i64)> = HashSet::new();
        for o in &orphelines {
            let Some(&piste) = cibles.get(&o.id) else {
                stats.unresolved += 1;
                continue;
            };
            if !deja.contains(&(o.playlist_id, piste)) && self.deja_dans(o.playlist_id, piste)? {
                let p: [&dyn ToSqlValue; 1] = [&o.id];
                self.db.execute(
                    "DELETE FROM playlist_tracks WHERE id = ? AND track_id IS NULL \
                     AND source IS NULL",
                    &p,
                )?;
                info!(
                    playlist_id = o.playlist_id,
                    track_id = piste,
                    "playlist_ligne_introuvable_absorbee"
                );
                stats.deduplicated += 1;
                continue;
            }
            let p: [&dyn ToSqlValue; 2] = [&piste, &o.id];
            let n = self.db.execute(
                "UPDATE playlist_tracks SET track_id = ?, title = NULL, artist = NULL, \
                 album = NULL, duration_ms = NULL, file_path = NULL, audio_hash = NULL, \
                 audio_fingerprint = NULL, mbid = NULL, missing_since = NULL \
                 WHERE id = ? AND track_id IS NULL AND source IS NULL AND source_id IS NULL",
                &p,
            )?;
            if n > 0 {
                deja.insert((o.playlist_id, piste));
                info!(
                    playlist_id = o.playlist_id,
                    track_id = piste,
                    title = %o.title,
                    "playlist_ligne_rattachee"
                );
                stats.relinked += 1;
            }
        }
        Ok(stats)
    }

    /// Une ligne locale dont la piste n'existe plus (base PostgreSQL basculée
    /// depuis SQLite : aucune clé étrangère, donc ni cascade ni `SET NULL`)
    /// devient introuvable. Son identité vient de l'historique d'écoute quand
    /// il la connaît, comme pour les favoris.
    fn detacher_les_pendantes(&self) -> Result<u64, String> {
        let rows = self.db.query_many(
            "SELECT pt.id, pt.track_id FROM playlist_tracks pt \
             WHERE pt.track_id IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.id = pt.track_id)",
            &[],
        )?;
        let maintenant = maintenant();
        let mut n = 0u64;
        for r in &rows {
            let (Some(ligne), Some(ancienne)) = (entier(r, 0), entier(r, 1)) else {
                continue;
            };
            let p: [&dyn ToSqlValue; 1] = [&ancienne];
            let histoire = self
                .db
                .query_one(
                    "SELECT title, artist_name, album_title FROM listen_history \
                     WHERE track_id = ? AND title IS NOT NULL AND title != '' \
                     ORDER BY id DESC LIMIT 1",
                    &p,
                )
                .unwrap_or(None);
            let (titre, artiste, album) = match &histoire {
                Some(h) => (texte(h, 0), texte(h, 1), texte(h, 2)),
                None => (None, None, None),
            };
            let p: [&dyn ToSqlValue; 6] =
                [&maintenant, &titre, &artiste, &album, &ligne, &ancienne];
            n += self.db.execute(
                "UPDATE playlist_tracks SET track_id = NULL, missing_since = ?, \
                 title = COALESCE(?, title), artist = COALESCE(?, artist), \
                 album = COALESCE(?, album) \
                 WHERE id = ? AND track_id = ? AND source IS NULL",
                &p,
            )? as u64;
            warn!(
                ligne,
                track_id = ancienne,
                "playlist_ligne_pendante_detachee"
            );
        }
        Ok(n)
    }

    fn orphelines(&self) -> Result<Vec<Orpheline>, String> {
        let sql = format!(
            "SELECT pt.id, pt.playlist_id, pt.title, pt.artist, pt.album, pt.duration_ms \
             FROM playlist_tracks pt WHERE {INTROUVABLE} ORDER BY pt.playlist_id, pt.position, pt.id"
        );
        Ok(self
            .db
            .query_many(&sql, &[])?
            .iter()
            .filter_map(|r| {
                Some(Orpheline {
                    id: entier(r, 0)?,
                    playlist_id: entier(r, 1)?,
                    title: texte(r, 2).unwrap_or_default(),
                    artist: texte(r, 3).unwrap_or_default(),
                    album: texte(r, 4).unwrap_or_default(),
                    duration_ms: entier(r, 5),
                })
            })
            .collect())
    }

    /// Règles 1 à 3 : `(ligne, piste)` par égalité de `colonne` (même nom
    /// des deux côtés). Le plus petit id gagne : deux pistes au même chemin
    /// n'existent pas (`UNIQUE`), et deux pistes au même contenu sont des
    /// copies l'une de l'autre.
    fn par_jointure(&self, colonne: &str) -> Result<Vec<(i64, i64)>, String> {
        let sql = format!(
            "SELECT pt.id, MIN(t.id) FROM playlist_tracks pt \
             JOIN tracks t ON t.{colonne} = pt.{colonne} \
             WHERE {INTROUVABLE} AND pt.{colonne} IS NOT NULL AND pt.{colonne} != '' \
             GROUP BY pt.id"
        );
        Ok(self
            .db
            .query_many(&sql, &[])?
            .iter()
            .filter_map(|r| Some((entier(r, 0)?, entier(r, 1)?)))
            .collect())
    }

    /// Règle 4 : les candidats par MBID d'enregistrement, avec leur album.
    /// L'expression est celle de l'index `idx_tracks_mbid_recording_norm`.
    fn candidats_mbid(&self) -> Result<HashMap<i64, Vec<Candidat>>, String> {
        let sql = format!(
            "SELECT pt.id, t.id, COALESCE(al.title, ''), t.duration_ms \
             FROM playlist_tracks pt \
             JOIN tracks t ON {mbid} = LOWER(TRIM(pt.mbid)) \
             LEFT JOIN albums al ON al.id = t.album_id \
             WHERE {INTROUVABLE} AND pt.mbid IS NOT NULL AND TRIM(pt.mbid) != '' \
             ORDER BY pt.id, t.id",
            mbid = super::migrations::SQL_MBID_PLIE,
        );
        let mut out: HashMap<i64, Vec<Candidat>> = HashMap::new();
        for r in self.db.query_many(&sql, &[])? {
            let (Some(ligne), Some(piste)) = (entier(&r, 0), entier(&r, 1)) else {
                continue;
            };
            out.entry(ligne).or_default().push(Candidat {
                id: piste,
                artiste: String::new(),
                album: plier(&texte(&r, 2).unwrap_or_default()),
                duree: entier(&r, 3),
            });
        }
        Ok(out)
    }

    /// Règle 5 : les pistes de la bibliothèque dont le titre plié figure dans
    /// `titres`, rangées par titre plié. Une lecture, pour toute la passe.
    fn pistes_par_titre(
        &self,
        titres: &HashSet<String>,
    ) -> Result<HashMap<String, Vec<Candidat>>, String> {
        let rows = self.db.query_many(
            "SELECT t.id, t.title, COALESCE(ar.name, ''), COALESCE(al.title, ''), t.duration_ms \
             FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id \
             LEFT JOIN albums al ON al.id = t.album_id ORDER BY t.id",
            &[],
        )?;
        let mut out: HashMap<String, Vec<Candidat>> = HashMap::new();
        for r in &rows {
            let titre = plier(&texte(r, 1).unwrap_or_default());
            if !titres.contains(&titre) {
                continue;
            }
            let Some(id) = entier(r, 0) else { continue };
            out.entry(titre).or_default().push(Candidat {
                id,
                artiste: plier(&texte(r, 2).unwrap_or_default()),
                album: plier(&texte(r, 3).unwrap_or_default()),
                duree: entier(r, 4),
            });
        }
        Ok(out)
    }

    fn deja_dans(&self, playlist_id: i64, piste: i64) -> Result<bool, String> {
        let p: [&dyn ToSqlValue; 2] = [&playlist_id, &piste];
        Ok(self
            .db
            .query_one(
                "SELECT 1 FROM playlist_tracks WHERE playlist_id = ? AND track_id = ? LIMIT 1",
                &p,
            )?
            .is_some())
    }
}

/// La passe et son journal, en une ligne pour chaque ancrage : démarrage
/// (`complete`), fin de scan (`complete`), purge (`complete`), lot du
/// surveillant (rapide). `contexte` nomme l'ancrage dans le journal. Une
/// erreur est journalisée, jamais rendue : elle ne doit pas faire échouer le
/// scan qui l'appelle, et la passe suivante réessaie.
pub fn reconcilier_et_journaliser(
    db: Arc<dyn DbBackend>,
    complete: bool,
    contexte: &str,
) -> PlaylistsReconcileStats {
    let r = PlaylistsReconciler::with_backend(db);
    match if complete { r.run() } else { r.run_rapide() } {
        Ok(stats) => {
            if stats.changed() > 0 || stats.unresolved > 0 {
                info!(
                    contexte,
                    scanned = stats.scanned,
                    relinked = stats.relinked,
                    deduplicated = stats.deduplicated,
                    detached = stats.detached,
                    unresolved = stats.unresolved,
                    "playlists_reconciled"
                );
            }
            stats
        }
        Err(e) => {
            warn!(contexte, error = %e, "playlists_reconcile_failed");
            PlaylistsReconcileStats::default()
        }
    }
}

/// Une piste candidate au rattachement (règles 4 et 5).
#[derive(Debug, Clone)]
struct Candidat {
    id: i64,
    artiste: String,
    album: String,
    duree: Option<i64>,
}

/// Le gagnant UNIQUE parmi `candidats`, ou aucun : seul, sinon seul sur le
/// même album, sinon seul à ±2 s de la durée connue (sur le même album s'il
/// y en a, sinon parmi tous).
fn trancher(candidats: &[Candidat], album: &str, duree: Option<i64>) -> Option<i64> {
    match candidats {
        [] => return None,
        [seul] => return Some(seul.id),
        _ => {}
    }
    let album = plier(album);
    let memes_albums: Vec<&Candidat> = if album.is_empty() {
        Vec::new()
    } else {
        candidats.iter().filter(|c| c.album == album).collect()
    };
    if let [seul] = memes_albums.as_slice() {
        return Some(seul.id);
    }
    let duree = duree.filter(|d| *d > 0)?;
    let base: Vec<&Candidat> = if memes_albums.is_empty() {
        candidats.iter().collect()
    } else {
        memes_albums
    };
    let proches: Vec<&&Candidat> = base
        .iter()
        .filter(|c| {
            c.duree
                .is_some_and(|d| (d - duree).abs() <= ECART_DE_DUREE_MS)
        })
        .collect();
    match proches.as_slice() {
        [seul] => Some(seul.id),
        _ => None,
    }
}

/// Comparaison insensible à la casse et aux espaces de bord.
fn plier(s: &str) -> String {
    s.trim().to_lowercase()
}

fn entier(r: &[SqlValue], i: usize) -> Option<i64> {
    r.get(i).and_then(|v| v.as_i64())
}

fn texte(r: &[SqlValue], i: usize) -> Option<String> {
    r.get(i).and_then(|v| v.as_string())
}

fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "playlists_reconcile_tests.rs"]
mod tests;
