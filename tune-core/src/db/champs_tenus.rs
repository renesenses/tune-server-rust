//! Les CHAMPS TENUS d'une piste — ce que l'utilisateur a corrigé à la main, et
//! qu'aucune analyse ne doit défaire.
//!
//! Bertrand, 05/10/2026 : depuis que l'écriture dans les fichiers audio est
//! désactivée par défaut (`metadata::ecriture_fichiers`), une correction faite
//! dans Tune ne vit plus qu'en base. Or une « Analyse complète » reconstruit
//! chaque ligne de piste à partir des balises du fichier : sans mémoire, le
//! genre, l'année ou le compositeur corrigés revenaient aux valeurs du
//! fichier. Les champs édités à la main sont donc **tenus** : mémorisés ici,
//! et reposés sur la ligne après chaque lecture des balises — au même point
//! que les tenues de la fiche album ([`super::edition_album::Tenues`], qui
//! charge ce registre et l'applique en dernier).
//!
//! « Rétablir depuis le fichier » efface la tenue d'une piste ([`retablir`]) ;
//! la prochaine relecture reprend alors les balises.
//!
//! ## Stockage
//!
//! Une ligne `track_metadata` par piste, clé [`CLE`], valeur JSON
//! [`ChampsTenus`]. Pas de migration : la table existe sur SQLite et sur
//! PostgreSQL. Le JSON porte le CHEMIN du fichier (ou l'identité CUE) : c'est
//! par lui que le scan retrouve la tenue, comme pour la fiche album.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::models::Track;
use super::track_metadata_repo::TrackMetadataRepo;

/// Clé de la ligne `track_metadata` qui porte la tenue d'une piste.
pub const CLE: &str = "champs_tenus";

/// Les champs d'une piste tenus à la main. `None` : non tenu, la balise du
/// fichier décide.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChampsTenus {
    /// Chemin du fichier au moment de la tenue (clé de recherche du scan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chemin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue_media: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue_debut_ms: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    /// `tracks.genres` (tableau JSON) tel qu'il était après l'édition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genres: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Un champ éditable d'une piste — ce que les routes d'édition déclarent
/// avoir modifié.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Champ {
    Titre,
    Artiste,
    Album,
    ArtisteAlbum,
    Genre,
    NumeroPiste,
    NumeroDisque,
    Annee,
    Compositeur,
    Label,
}

impl ChampsTenus {
    /// Les noms des champs tenus, pour l'interface.
    pub fn noms(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.title.is_some() {
            v.push("title");
        }
        if self.artist_id.is_some() || self.artist_name.is_some() {
            v.push("artist");
        }
        if self.album_id.is_some() || self.album_title.is_some() {
            v.push("album");
        }
        if self.album_artist.is_some() {
            v.push("album_artist");
        }
        if self.genre.is_some() {
            v.push("genre");
        }
        if self.track_number.is_some() {
            v.push("track_number");
        }
        if self.disc_number.is_some() {
            v.push("disc_number");
        }
        if self.year.is_some() {
            v.push("year");
        }
        if self.composer.is_some() {
            v.push("composer");
        }
        if self.label.is_some() {
            v.push("label");
        }
        v
    }

    pub fn est_vide(&self) -> bool {
        self.noms().is_empty()
    }

    /// Recopie depuis la ligne `track` (APRÈS l'édition) les champs listés.
    fn tenir_depuis(&mut self, track: &Track, champs: &[Champ]) {
        for c in champs {
            match c {
                Champ::Titre => self.title = Some(track.title.clone()),
                Champ::Artiste => {
                    self.artist_id = track.artist_id;
                    self.artist_name = track.artist_name.clone();
                }
                Champ::Album => {
                    self.album_id = track.album_id;
                    self.album_title = track.album_title.clone();
                }
                Champ::ArtisteAlbum => self.album_artist = track.album_artist.clone(),
                Champ::Genre => {
                    self.genre = track.genre.clone();
                    self.genres = track.genres.clone();
                }
                Champ::NumeroPiste => self.track_number = Some(track.track_number),
                Champ::NumeroDisque => self.disc_number = Some(track.disc_number),
                Champ::Annee => self.year = track.year,
                Champ::Compositeur => self.composer = track.composer.clone(),
                Champ::Label => self.label = track.label.clone(),
            }
        }
    }

    /// Pose la tenue sur une ligne. Rend vrai si un champ est tenu.
    fn poser(&self, track: &mut Track, vivants: &Vivants) -> bool {
        let mut pose = false;
        if let Some(v) = &self.title {
            track.title = v.clone();
            pose = true;
        }
        if self.artist_id.is_some() || self.artist_name.is_some() {
            // Un artiste supprimé depuis ne se repose pas : la clé étrangère
            // ferait tomber l'écriture de la ligne.
            if let Some(a) = self.artist_id.filter(|a| vivants.artistes.contains(a)) {
                track.artist_id = Some(a);
            }
            if let Some(n) = &self.artist_name {
                track.artist_name = Some(n.clone());
            }
            pose = true;
        }
        if self.album_id.is_some() || self.album_title.is_some() {
            if let Some(a) = self.album_id.filter(|a| vivants.albums.contains(a)) {
                track.album_id = Some(a);
            }
            if let Some(t) = &self.album_title {
                track.album_title = Some(t.clone());
            }
            pose = true;
        }
        if let Some(v) = &self.album_artist {
            track.album_artist = Some(v.clone());
            pose = true;
        }
        if let Some(v) = &self.genre {
            track.genre = Some(v.clone());
            track.genres = self.genres.clone();
            pose = true;
        }
        if let Some(v) = self.track_number {
            track.track_number = v;
            pose = true;
        }
        if let Some(v) = self.disc_number {
            track.disc_number = v;
            pose = true;
        }
        if let Some(v) = self.year {
            track.year = Some(v);
            pose = true;
        }
        if let Some(v) = &self.composer {
            track.composer = Some(v.clone());
            pose = true;
        }
        if let Some(v) = &self.label {
            track.label = Some(v.clone());
            pose = true;
        }
        pose
    }
}

#[derive(Clone, Debug, Default)]
struct Vivants {
    artistes: HashSet<i64>,
    albums: HashSet<i64>,
}

/// Toutes les tenues de champs, par chemin et par identité CUE. Chargé UNE
/// fois par scan, avec les tenues de la fiche album.
#[derive(Clone, Debug, Default)]
pub struct Registre {
    par_chemin: HashMap<String, ChampsTenus>,
    par_cue: HashMap<(String, i64), ChampsTenus>,
    vivants: Vivants,
}

fn marque(engine: Engine, n: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

fn ids_existants(db: &Arc<dyn DbBackend>, table: &str, ids: &BTreeSet<i64>) -> HashSet<i64> {
    let mut vus = HashSet::new();
    if ids.is_empty() {
        return vus;
    }
    let liste = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    match db.query_many(
        &format!("SELECT id FROM {table} WHERE id IN ({liste})"),
        &[],
    ) {
        Ok(rows) => {
            for r in rows {
                if let Some(i) = r.first().and_then(|v| v.as_i64()) {
                    vus.insert(i);
                }
            }
        }
        Err(e) => tracing::warn!(table, erreur = %e, "champs_tenus_vivants_illisibles"),
    }
    vus
}

impl Registre {
    /// Un défaut de lecture rend un registre VIDE en le disant au journal : un
    /// scan ne s'arrête pas sur une table de métadonnées illisible.
    pub fn charger(db: &Arc<dyn DbBackend>) -> Self {
        let sql = format!(
            "SELECT value FROM track_metadata WHERE key = {}",
            marque(db.engine(), 1)
        );
        let rows = match db.query_many(&sql, &[&CLE as &dyn ToSqlValue]) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(erreur = %e, "champs_tenus_illisibles");
                return Self::default();
            }
        };
        let mut r = Self::default();
        let mut artistes = BTreeSet::new();
        let mut albums = BTreeSet::new();
        for row in rows {
            let Some(v) = row.first().and_then(|v| v.as_string()) else {
                continue;
            };
            let Ok(t) = serde_json::from_str::<ChampsTenus>(&v) else {
                continue;
            };
            if t.est_vide() {
                continue;
            }
            artistes.extend(t.artist_id);
            albums.extend(t.album_id);
            if let (Some(m), Some(d)) = (t.cue_media.clone(), t.cue_debut_ms) {
                r.par_cue.insert((m, d), t.clone());
            }
            if let Some(c) = t.chemin.clone() {
                r.par_chemin.insert(c, t);
            }
        }
        r.vivants = Vivants {
            artistes: ids_existants(db, "artists", &artistes),
            albums: ids_existants(db, "albums", &albums),
        };
        r
    }

    pub fn is_empty(&self) -> bool {
        self.par_chemin.is_empty() && self.par_cue.is_empty()
    }

    fn de_la_piste(&self, track: &Track) -> Option<&ChampsTenus> {
        track
            .file_path
            .as_deref()
            .and_then(|c| self.par_chemin.get(c))
            .or_else(|| {
                let media = track.cue_media_path.as_deref()?;
                self.par_cue.get(&(media.to_string(), track.cue_start_ms?))
            })
    }

    /// Pose sur une ligne construite depuis les balises les champs tenus à la
    /// main. Rend vrai si la ligne a changé.
    pub fn appliquer(&self, track: &mut Track) -> bool {
        match self.de_la_piste(track) {
            Some(t) => t.poser(track, &self.vivants),
            None => false,
        }
    }
}

fn lire(db: &Arc<dyn DbBackend>, track_id: i64) -> Option<ChampsTenus> {
    let tout = TrackMetadataRepo::with_backend(db.clone())
        .get_all(track_id)
        .ok()?;
    serde_json::from_str(tout.get(CLE)?).ok()
}

/// La tenue d'une piste, si elle en a une.
pub fn de_la_piste(db: &Arc<dyn DbBackend>, track_id: i64) -> Option<ChampsTenus> {
    lire(db, track_id).filter(|t| !t.est_vide())
}

/// Tient les `champs` de la piste `track` (relue APRÈS l'édition) : ils
/// survivront à toute relecture des balises. Les champs déjà tenus restent.
pub fn tenir(db: &Arc<dyn DbBackend>, track: &Track, champs: &[Champ]) -> Result<(), String> {
    let Some(id) = track.id else {
        return Err("piste sans identifiant".into());
    };
    if champs.is_empty() {
        return Ok(());
    }
    let mut t = lire(db, id).unwrap_or_default();
    t.chemin = track.file_path.clone();
    t.cue_media = track.cue_media_path.clone();
    t.cue_debut_ms = track.cue_start_ms;
    t.tenir_depuis(track, champs);
    let json = serde_json::to_string(&t).map_err(|e| e.to_string())?;
    TrackMetadataRepo::with_backend(db.clone()).set(id, CLE, &json)
}

/// Tient les champs listés pour une piste désignée par son identifiant (la
/// ligne est relue en base). Une erreur est journalisée, jamais rendue : la
/// modification en base est faite, seule sa survie à l'analyse est en jeu.
pub fn tenir_par_id(db: &Arc<dyn DbBackend>, track_id: i64, champs: &[Champ]) {
    let repo = super::track_repo::TrackRepo::with_backend(db.clone());
    let resultat = match repo.get(track_id) {
        Ok(Some(t)) => tenir(db, &t, champs),
        Ok(None) => return,
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = resultat {
        tracing::warn!(track_id, erreur = %e, "champs_tenus_non_retenus");
    }
}

/// « Rétablir depuis le fichier » : la piste n'a plus de champ tenu. Rend vrai
/// si elle en avait.
pub fn retablir(db: &Arc<dyn DbBackend>, track_id: i64) -> Result<bool, String> {
    let avait = lire(db, track_id).is_some();
    TrackMetadataRepo::with_backend(db.clone()).delete(track_id, CLE)?;
    Ok(avait)
}

#[cfg(test)]
#[path = "champs_tenus_tests.rs"]
mod tests;
