//! Édition d'un album, de ses disques et de ses pistes — GO de Bertrand du
//! 25/09/2026, chantier « édition et modification des albums, compilations et
//! coffrets ».
//!
//! # Ce que l'utilisateur modifie
//!
//! Depuis le mode « Modifier » de la fiche album : les champs de l'album
//! (titre, artiste d'album, année, label, genre, type de sortie), le mode de
//! compilation (`auto` / `oui` / `non`), l'ORDRE des disques et leur NOM,
//! l'ordre des pistes dans chaque disque — une piste peut changer de disque —,
//! le titre et l'artiste de chaque piste. Et deux gestes sur les disques :
//! en ATTACHER un (un autre album devient le disque suivant du coffret) ou en
//! DÉTACHER un (le disque redevient un album séparé).
//!
//! # Où ça vit — sans migration
//!
//! Tout tient dans les colonnes et le magasin clé-valeur existants :
//!
//! - `albums.*` pour les champs de l'album ; `tracks.disc_number`,
//!   `tracks.track_number`, `tracks.disc_subtitle` (le NOM du disque, balise
//!   DISCSUBTITLE, déjà affiché par la fiche web) et `tracks.title` /
//!   `tracks.artist_id` pour les pistes ;
//! - `album_metadata.edition_manuelle` (C3, [`CLE_EDITION_MANUELLE`]) nomme
//!   les champs d'ALBUM tenus à la main — c'est ce que lisent déjà le scan
//!   (`mark_compilation`, `reclasser_en_compilation`), le recalcul des
//!   compilations et la passe des coffrets automatiques ;
//! - `album_metadata.edition_pistes` ([`CLE_EDITION_PISTES`]) retient, piste
//!   par piste, ce que l'utilisateur a DISPOSÉ (album, disque, numéro, nom du
//!   disque) et ce qu'il a RENOMMÉ (titre, artiste de piste). La piste y est
//!   désignée par son CHEMIN : le surveillant de fichiers supprime puis recrée
//!   la ligne d'un fichier modifié, son identifiant ne survit pas ; son chemin
//!   si.
//!
//! # Pourquoi une analyse ne l'écrase plus
//!
//! Un scan qui relit un fichier (scan forcé, fichier modifié, surveillant)
//! reconstruit la ligne piste depuis les BALISES : numéro de disque, numéro de
//! piste, DISCSUBTITLE, titre, artiste — et l'album, résolu par le DOSSIER.
//! [`Tenues`] est chargé une fois par scan et appliqué à chaque ligne AVANT
//! qu'elle soit écrite (`TrackImporter::import`, et la voie du surveillant) :
//! la ligne écrite porte d'emblée la valeur de l'utilisateur. La passe des
//! coffrets automatiques épargne un album dont la disposition est tenue, le
//! recalcul des compilations épargne un album dont le mode est forcé.
//!
//! ⚠️ Un « réinitialiser la bibliothèque » (`DELETE FROM albums`) emporte
//! `album_metadata`, donc ces éditions, comme il emporte déjà toutes les
//! autres éditions manuelles. Rien n'est écrit dans les FICHIERS dans cette
//! tranche.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};

use super::album_metadata_repo::{AlbumMetadataRepo, CLE_EDITION_MANUELLE};
use super::album_repo::AlbumRepo;
use super::artist_repo::ArtistRepo;
use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::coffrets_auto::{self, CLE_COFFRET, Marqueur};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::models::Track;
use crate::TuneError;
use crate::library::regle_compilation::IndicesCompilation;

/// Clé, dans `album_metadata`, de la disposition et des renommages de pistes
/// tenus à la main. Valeur : un [`EditionPistes`] en JSON.
pub const CLE_EDITION_PISTES: &str = "edition_pistes";

/// Les trois modes de compilation que l'écran propose.
pub const MODE_AUTO: &str = "auto";
pub const MODE_OUI: &str = "oui";
pub const MODE_NON: &str = "non";

/// Nom, dans `edition_manuelle`, du drapeau « compilation » (C3, #4427).
const CHAMP_COMPILATION: &str = "is_compilation";

fn marque(engine: Engine, n: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// `Some(None)` pour un `null` explicite, `None` pour un champ absent : ce qui
/// distingue « effacer » de « ne pas toucher ».
pub fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

// ---------------------------------------------------------------------------
// Ce qui est retenu
// ---------------------------------------------------------------------------

/// Une piste dont quelque chose est tenu à la main.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PisteTenue {
    /// L'identifiant au moment de l'édition — informatif : le surveillant en
    /// change. Le chemin, lui, désigne la piste.
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chemin: Option<String>,
    pub disque: i32,
    pub numero: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nom_disque: Option<String>,
    /// Titre renommé à la main (absent = celui des balises).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub titre: Option<String>,
    /// Artiste de piste choisi à la main (absent = celui des balises).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artiste_id: Option<i64>,
}

/// La valeur de [`CLE_EDITION_PISTES`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditionPistes {
    /// Vrai quand l'utilisateur a DISPOSÉ les disques (ordre, noms, numéros,
    /// appartenance à l'album) : alors `disque`, `numero` et `nom_disque` de
    /// chaque piste sont tenus, et l'album avec. Faux quand seuls des titres
    /// ou des artistes de piste l'ont été.
    #[serde(default)]
    pub disposition: bool,
    #[serde(default)]
    pub pistes: Vec<PisteTenue>,
}

impl EditionPistes {
    fn a_des_renommages(&self) -> bool {
        self.pistes
            .iter()
            .any(|p| p.titre.is_some() || p.artiste_id.is_some())
    }
}

fn lire_edition(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<EditionPistes, TuneError> {
    Ok(AlbumMetadataRepo::with_backend(db.clone())
        .get_all(album_id)?
        .get(CLE_EDITION_PISTES)
        .and_then(|v| serde_json::from_str::<EditionPistes>(v).ok())
        .unwrap_or_default())
}

/// Une piste de l'album telle que la base la porte.
#[derive(Clone, Debug)]
struct Ligne {
    id: i64,
    chemin: Option<String>,
    disque: i32,
    numero: i32,
    nom_disque: Option<String>,
}

fn sql_lignes(engine: Engine) -> String {
    format!(
        "SELECT id, file_path, disc_number, track_number, disc_subtitle \
         FROM tracks WHERE album_id = {} ORDER BY id",
        marque(engine, 1)
    )
}

fn lignes_de(rows: Vec<Vec<SqlValue>>) -> Vec<Ligne> {
    rows.into_iter()
        .filter_map(|r| {
            Some(Ligne {
                id: r.first()?.as_i64()?,
                chemin: r
                    .get(1)
                    .and_then(|v| v.as_string())
                    .filter(|s| !s.is_empty()),
                disque: r.get(2).and_then(|v| v.as_i64()).unwrap_or(1) as i32,
                numero: r.get(3).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                nom_disque: r
                    .get(4)
                    .and_then(|v| v.as_string())
                    .filter(|s| !s.trim().is_empty()),
            })
        })
        .collect()
}

/// Reconstruit la valeur retenue d'un album depuis ses pistes ACTUELLES.
///
/// Les renommages déjà retenus (`heritage`, par identifiant de piste) sont
/// repris ; ceux de cette édition (`titres`, `artistes`) les remplacent. Une
/// piste qui n'est plus dans l'album n'y figure plus.
fn construire(
    lignes: &[Ligne],
    disposition: bool,
    heritage: &[&EditionPistes],
    titres: &HashMap<i64, String>,
    artistes: &HashMap<i64, i64>,
) -> EditionPistes {
    let mut anciens: HashMap<i64, &PisteTenue> = HashMap::new();
    let mut anciens_par_chemin: HashMap<&str, &PisteTenue> = HashMap::new();
    for e in heritage {
        for p in &e.pistes {
            anciens.insert(p.id, p);
            if let Some(c) = p.chemin.as_deref() {
                anciens_par_chemin.insert(c, p);
            }
        }
    }
    let pistes = lignes
        .iter()
        .filter_map(|l| {
            let ancien = anciens.get(&l.id).copied().or_else(|| {
                l.chemin
                    .as_deref()
                    .and_then(|c| anciens_par_chemin.get(c).copied())
            });
            let titre = titres
                .get(&l.id)
                .cloned()
                .or_else(|| ancien.and_then(|a| a.titre.clone()));
            let artiste_id = artistes
                .get(&l.id)
                .copied()
                .or_else(|| ancien.and_then(|a| a.artiste_id));
            if !disposition && titre.is_none() && artiste_id.is_none() {
                return None;
            }
            Some(PisteTenue {
                id: l.id,
                chemin: l.chemin.clone(),
                disque: l.disque,
                numero: l.numero,
                nom_disque: l.nom_disque.clone(),
                titre,
                artiste_id,
            })
        })
        .collect();
    EditionPistes {
        disposition,
        pistes,
    }
}

/// Écrit (ou retire, s'il n'y a plus rien à tenir) la valeur retenue.
fn ecrire_edition(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    e: &EditionPistes,
) -> Result<(), TuneError> {
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    if !e.disposition && !e.a_des_renommages() {
        meta.delete(album_id, CLE_EDITION_PISTES)?;
        return Ok(());
    }
    let json = serde_json::to_string(e).map_err(|x| TuneError::from(x.to_string()))?;
    meta.set(album_id, CLE_EDITION_PISTES, &json)?;
    Ok(())
}

/// Fige la disposition ACTUELLE d'un album (après attacher / détacher).
fn figer(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    heritage: &[&EditionPistes],
) -> Result<(), TuneError> {
    let rows = db.query_many_strong(&sql_lignes(db.engine()), &[&album_id as &dyn ToSqlValue])?;
    let e = construire(
        &lignes_de(rows),
        true,
        heritage,
        &HashMap::new(),
        &HashMap::new(),
    );
    ecrire_edition(db, album_id, &e)
}

// ---------------------------------------------------------------------------
// Les TENUES — ce que les analyses n'écrasent plus
// ---------------------------------------------------------------------------

/// Ce qu'une piste doit garder, quoi que disent ses balises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenue {
    pub album_id: i64,
    /// `(disque, numéro, nom du disque)` quand la disposition est tenue.
    pub disposition: Option<(i32, i32, Option<String>)>,
    pub titre: Option<String>,
    pub artiste_id: Option<i64>,
}

/// Toutes les tenues de la bibliothèque, par chemin de fichier. Chargé UNE
/// fois par scan : quelques albums édités, une requête.
#[derive(Clone, Debug, Default)]
pub struct Tenues {
    par_chemin: HashMap<String, Tenue>,
    albums_disposes: HashSet<i64>,
}

impl Tenues {
    /// Un défaut de lecture rend un ensemble VIDE en le disant au journal :
    /// on ne bloque pas un scan sur une table de métadonnées illisible.
    pub fn charger(db: &Arc<dyn DbBackend>) -> Self {
        match Self::essayer(db) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(erreur = %e, "editions_tenues_illisibles");
                Self::default()
            }
        }
    }

    fn essayer(db: &Arc<dyn DbBackend>) -> Result<Self, TuneError> {
        let sql = format!(
            "SELECT m.album_id, m.value FROM album_metadata m \
             JOIN albums a ON a.id = m.album_id WHERE m.key = {}",
            marque(db.engine(), 1)
        );
        let rows = db.query_many_strong(&sql, &[&CLE_EDITION_PISTES as &dyn ToSqlValue])?;
        let mut editions: Vec<(i64, EditionPistes)> = Vec::new();
        for r in rows {
            let (Some(id), Some(v)) = (
                r.first().and_then(|v| v.as_i64()),
                r.get(1).and_then(|v| v.as_string()),
            ) else {
                continue;
            };
            if let Ok(e) = serde_json::from_str::<EditionPistes>(&v) {
                editions.push((id, e));
            }
        }
        // Un artiste retenu qui n'existe plus ferait tomber l'écriture de la
        // piste sur la clé étrangère : on l'oublie plutôt.
        let artistes: BTreeSet<i64> = editions
            .iter()
            .flat_map(|(_, e)| e.pistes.iter().filter_map(|p| p.artiste_id))
            .collect();
        let mut vivants: HashSet<i64> = HashSet::new();
        if !artistes.is_empty() {
            let liste = artistes
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            for r in db.query_many_strong(
                &format!("SELECT id FROM artists WHERE id IN ({liste})"),
                &[],
            )? {
                if let Some(i) = r.first().and_then(|v| v.as_i64()) {
                    vivants.insert(i);
                }
            }
        }
        let mut t = Self::default();
        for (album_id, e) in editions {
            if e.disposition {
                t.albums_disposes.insert(album_id);
            }
            for p in e.pistes {
                let Some(chemin) = p.chemin else { continue };
                t.par_chemin.insert(
                    chemin,
                    Tenue {
                        album_id,
                        disposition: e
                            .disposition
                            .then(|| (p.disque, p.numero, p.nom_disque.clone())),
                        titre: p.titre,
                        artiste_id: p.artiste_id.filter(|a| vivants.contains(a)),
                    },
                );
            }
        }
        Ok(t)
    }

    pub fn is_empty(&self) -> bool {
        self.par_chemin.is_empty()
    }

    pub fn get(&self, chemin: &str) -> Option<&Tenue> {
        self.par_chemin.get(chemin)
    }

    /// Les albums dont la disposition des disques est tenue à la main.
    pub fn albums_disposes(&self) -> &HashSet<i64> {
        &self.albums_disposes
    }

    /// Pose sur une ligne piste — construite depuis les balises, pas encore
    /// écrite — ce que l'utilisateur a tenu. Rend vrai si la ligne a changé.
    pub fn appliquer(&self, track: &mut Track) -> bool {
        let Some(t) = track
            .file_path
            .as_deref()
            .and_then(|c| self.par_chemin.get(c))
        else {
            return false;
        };
        if let Some((disque, numero, nom)) = &t.disposition {
            track.album_id = Some(t.album_id);
            track.disc_number = *disque;
            track.track_number = *numero;
            track.disc_subtitle = nom.clone();
        }
        if let Some(titre) = &t.titre {
            track.title = titre.clone();
        }
        if let Some(a) = t.artiste_id {
            track.artist_id = Some(a);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// La vue d'édition — `GET /library/albums/{id}/edition`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VueAlbum {
    pub id: i64,
    pub title: String,
    pub album_artist: Option<String>,
    pub year: Option<i32>,
    pub label: Option<String>,
    pub genre: Option<String>,
    pub release_type: Option<String>,
    pub cover_path: Option<String>,
    pub compilation_mode: String,
    pub compilation_effective: bool,
    pub coffret: Option<String>,
    pub champs_edites: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VueDisque {
    pub number: i32,
    pub title: Option<String>,
    /// Pas de pochette par disque dans cette tranche : toujours `null`.
    pub cover_path: Option<String>,
    pub track_count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VuePiste {
    pub id: i64,
    pub disc_number: i32,
    pub track_number: i32,
    pub title: String,
    pub artist_name: Option<String>,
    pub duration_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VueEdition {
    pub album: VueAlbum,
    pub discs: Vec<VueDisque>,
    pub tracks: Vec<VuePiste>,
}

/// Les noms de `edition_manuelle` rendus sous ceux du contrat de l'écran.
fn nom_du_contrat(champ: &str) -> String {
    match champ {
        "artist" => "album_artist".into(),
        CHAMP_COMPILATION => "compilation_mode".into(),
        autre => autre.into(),
    }
}

/// La vue d'édition d'un album, `None` s'il n'existe pas.
pub fn lire_vue(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<VueEdition>, TuneError> {
    let Some(album) = AlbumRepo::with_backend(db.clone()).get(album_id)? else {
        return Ok(None);
    };
    let meta = AlbumMetadataRepo::with_backend(db.clone()).get_all(album_id)?;
    let tenus: Vec<String> = meta
        .get(CLE_EDITION_MANUELLE)
        .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
        .unwrap_or_default();
    let edition = meta
        .get(CLE_EDITION_PISTES)
        .and_then(|v| serde_json::from_str::<EditionPistes>(v).ok())
        .unwrap_or_default();
    let coffret = meta
        .get(CLE_COFFRET)
        .and_then(|v| serde_json::from_str::<Marqueur>(v).ok())
        .map(|m| m.origine);
    let compilation_mode = if tenus.iter().any(|c| c == CHAMP_COMPILATION) {
        if album.is_compilation {
            MODE_OUI
        } else {
            MODE_NON
        }
    } else {
        MODE_AUTO
    };
    let mut champs: BTreeSet<String> = tenus.iter().map(|c| nom_du_contrat(c)).collect();
    if edition.disposition {
        champs.insert("discs".into());
    }
    if edition.a_des_renommages() {
        champs.insert("tracks".into());
    }

    let p1 = marque(db.engine(), 1);
    let rows = db.query_many_strong(
        &format!(
            "SELECT t.id, t.disc_number, t.track_number, t.title, ar.name, t.duration_ms, \
             t.disc_subtitle FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id \
             WHERE t.album_id = {p1} \
             ORDER BY COALESCE(t.disc_number, 1), COALESCE(t.track_number, 0), t.id"
        ),
        &[&album_id as &dyn ToSqlValue],
    )?;
    let mut tracks = Vec::with_capacity(rows.len());
    let mut discs: Vec<VueDisque> = Vec::new();
    for r in rows {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let disc_number = r.get(1).and_then(|v| v.as_i64()).unwrap_or(1) as i32;
        let nom = r
            .get(6)
            .and_then(|v| v.as_string())
            .filter(|s| !s.trim().is_empty());
        match discs.last_mut() {
            Some(d) if d.number == disc_number => {
                d.track_count += 1;
                if d.title.is_none() {
                    d.title = nom;
                }
            }
            _ => discs.push(VueDisque {
                number: disc_number,
                title: nom,
                cover_path: None,
                track_count: 1,
            }),
        }
        tracks.push(VuePiste {
            id,
            disc_number,
            track_number: r.get(2).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            title: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
            artist_name: r.get(4).and_then(|v| v.as_string()),
            duration_ms: r.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
        });
    }
    Ok(Some(VueEdition {
        album: VueAlbum {
            id: album_id,
            title: album.title,
            album_artist: album.artist_name,
            year: album.year,
            label: album.label,
            genre: album.genre,
            release_type: album.release_type,
            cover_path: album.cover_path,
            compilation_mode: compilation_mode.into(),
            compilation_effective: album.is_compilation,
            coffret,
            champs_edites: champs.into_iter().collect(),
        },
        discs,
        tracks,
    }))
}

// ---------------------------------------------------------------------------
// Modifier — `PUT /library/albums/{id}/edition`
// ---------------------------------------------------------------------------

/// Un disque dans l'ordre FINAL : sa place dans la liste est son numéro.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DisqueModifie {
    /// Le numéro ACTUEL du disque — sert à garder son nom quand `title` est
    /// absent.
    #[serde(default)]
    pub number: Option<i32>,
    /// Absent : le nom actuel du disque `number` ; `null` : aucun nom.
    #[serde(default, deserialize_with = "present")]
    pub title: Option<Option<String>>,
    /// Les pistes du disque, dans l'ordre final (numéros réécrits 1..n).
    pub track_ids: Vec<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PisteModifiee {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist_name: Option<String>,
}

/// Le corps de `PUT /library/albums/{id}/edition` — tout est facultatif.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Modification {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub year: Option<Option<i32>>,
    #[serde(default, deserialize_with = "present")]
    pub label: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub genre: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub release_type: Option<Option<String>>,
    #[serde(default)]
    pub compilation_mode: Option<String>,
    #[serde(default)]
    pub discs: Option<Vec<DisqueModifie>>,
    #[serde(default)]
    pub tracks: Option<Vec<PisteModifiee>>,
}

/// Pourquoi une édition est refusée.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusEdition {
    AlbumInconnu(i64),
    /// 422 : la requête est lisible mais incohérente. `code` est stable.
    Invalide {
        code: &'static str,
        message: String,
    },
    Base(String),
}

impl From<TuneError> for RefusEdition {
    fn from(e: TuneError) -> Self {
        Self::Base(e.to_string())
    }
}

impl From<String> for RefusEdition {
    fn from(e: String) -> Self {
        Self::Base(e)
    }
}

fn invalide(code: &'static str, message: impl Into<String>) -> RefusEdition {
    RefusEdition::Invalide {
        code,
        message: message.into(),
    }
}

/// Un texte obligatoire : rogné, jamais vide.
fn texte_requis(v: &str, code: &'static str, quoi: &str) -> Result<String, RefusEdition> {
    let t = v.trim();
    if t.is_empty() {
        return Err(invalide(code, format!("{quoi} ne peut pas être vide")));
    }
    Ok(t.to_string())
}

/// Un texte facultatif : `null` ou vide efface.
fn texte_libre(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn artiste_nomme(artistes: &ArtistRepo, nom: &str) -> Result<i64, RefusEdition> {
    if let Some(a) = artistes.get_by_name(nom)?
        && let Some(id) = a.id
    {
        return Ok(id);
    }
    artistes
        .get_or_create(nom, None, None)?
        .id
        .ok_or_else(|| RefusEdition::Base(format!("artiste « {nom} » non créé")))
}

/// Applique une modification, en UNE transaction, et marque chaque champ
/// touché comme édité à la main.
///
/// Tout est VÉRIFIÉ avant la première écriture : une piste manquante, en
/// double ou étrangère à l'album, un titre vide, un mode inconnu refusent la
/// requête entière (422) sans rien écrire.
pub fn appliquer(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    m: &Modification,
) -> Result<(), RefusEdition> {
    let repo = AlbumRepo::with_backend(db.clone());
    if repo.get(album_id)?.is_none() {
        return Err(RefusEdition::AlbumInconnu(album_id));
    }
    let engine = db.engine();
    let lignes =
        lignes_de(db.query_many_strong(&sql_lignes(engine), &[&album_id as &dyn ToSqlValue])?);
    let ids_album: HashSet<i64> = lignes.iter().map(|l| l.id).collect();

    // --- 1. Vérifier ---------------------------------------------------
    let titre = m
        .title
        .as_deref()
        .map(|t| texte_requis(t, "titre_vide", "le titre de l'album"))
        .transpose()?;
    let nom_artiste = m
        .album_artist
        .as_deref()
        .map(|t| texte_requis(t, "artiste_vide", "l'artiste de l'album"))
        .transpose()?;
    let mode = match m.compilation_mode.as_deref().map(str::trim) {
        None => None,
        Some(v @ (MODE_AUTO | MODE_OUI | MODE_NON)) => Some(v.to_string()),
        Some(autre) => {
            return Err(invalide(
                "mode_de_compilation_inconnu",
                format!("compilation_mode « {autre} » : attendu auto, oui ou non"),
            ));
        }
    };
    // Les disques : l'ordre final, chaque piste de l'album exactement une fois.
    //
    // Un disque VIDÉ (toutes ses pistes déplacées ailleurs) n'est pas envoyé
    // par l'écran — ou l'est sans piste : il disparaît, et les autres sont
    // numérotés 1..n dans l'ordre reçu. C'est le disque qui disparaît, jamais
    // une piste : une piste d'un disque absent qui ne figure nulle part ailleurs
    // reste un refus (`piste_manquante`).
    let mut disposition: Vec<(i32, Option<String>, Vec<i64>)> = Vec::new();
    if let Some(discs) = &m.discs {
        let discs: Vec<&DisqueModifie> = discs.iter().filter(|d| !d.track_ids.is_empty()).collect();
        if discs.is_empty() {
            return Err(invalide(
                "disques_vides",
                "la liste des disques ne peut pas être vide : l'album a des pistes",
            ));
        }
        let mut vues: HashSet<i64> = HashSet::new();
        for (rang, d) in discs.iter().enumerate() {
            for id in &d.track_ids {
                if !ids_album.contains(id) {
                    return Err(invalide(
                        "piste_etrangere",
                        format!("la piste {id} n'appartient pas à l'album {album_id}"),
                    ));
                }
                if !vues.insert(*id) {
                    return Err(invalide(
                        "piste_en_double",
                        format!("la piste {id} figure plus d'une fois"),
                    ));
                }
            }
            let nom = match &d.title {
                Some(t) => texte_libre(t),
                None => d.number.and_then(|n| {
                    lignes
                        .iter()
                        .find(|l| l.disque == n && l.nom_disque.is_some())
                        .and_then(|l| l.nom_disque.clone())
                }),
            };
            disposition.push(((rang + 1) as i32, nom, d.track_ids.clone()));
        }
        let mut manquantes: Vec<i64> = ids_album.difference(&vues).copied().collect();
        if !manquantes.is_empty() {
            manquantes.sort_unstable();
            return Err(invalide(
                "piste_manquante",
                format!(
                    "chaque piste de l'album doit figurer exactement une fois ; absentes : {}",
                    manquantes
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
    }
    let mut titres_pistes: HashMap<i64, String> = HashMap::new();
    let mut noms_artistes_pistes: Vec<(i64, String)> = Vec::new();
    for p in m.tracks.iter().flatten() {
        if !ids_album.contains(&p.id) {
            return Err(invalide(
                "piste_etrangere",
                format!("la piste {} n'appartient pas à l'album {album_id}", p.id),
            ));
        }
        if let Some(t) = &p.title {
            titres_pistes.insert(
                p.id,
                texte_requis(t, "titre_de_piste_vide", "le titre d'une piste")?,
            );
        }
        if let Some(a) = &p.artist_name {
            noms_artistes_pistes.push((
                p.id,
                texte_requis(a, "artiste_de_piste_vide", "l'artiste d'une piste")?,
            ));
        }
    }

    // --- 2. Résoudre les artistes (hors transaction : créer un artiste est
    //        sans conséquence si l'édition échoue ensuite) -----------------
    let artistes = ArtistRepo::with_backend(db.clone());
    let artiste_album = nom_artiste
        .as_deref()
        .map(|n| artiste_nomme(&artistes, n))
        .transpose()?;
    let mut artistes_pistes: HashMap<i64, i64> = HashMap::new();
    for (id, nom) in &noms_artistes_pistes {
        artistes_pistes.insert(*id, artiste_nomme(&artistes, nom)?);
    }

    // --- 3. Ce qui sera marqué ------------------------------------------
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    let mut tenus: BTreeSet<String> = meta
        .champs_edites_a_la_main(album_id)?
        .into_iter()
        .collect();
    let tenus_avant = tenus.clone();
    for (touche, champ) in [
        (titre.is_some(), "title"),
        (artiste_album.is_some(), "artist"),
        (m.year.is_some(), "year"),
        (m.label.is_some(), "label"),
        (m.genre.is_some(), "genre"),
        (m.release_type.is_some(), "release_type"),
    ] {
        if touche {
            tenus.insert(champ.to_string());
        }
    }
    match mode.as_deref() {
        Some(MODE_AUTO) => {
            tenus.remove(CHAMP_COMPILATION);
        }
        Some(_) => {
            tenus.insert(CHAMP_COMPILATION.to_string());
        }
        None => {}
    }
    let precedent = lire_edition(db, album_id)?;
    let touche_pistes = m.discs.is_some() || m.tracks.is_some();

    // --- 4. Écrire, en une transaction -------------------------------------
    let p = |n| marque(engine, n);
    let (p1, p2, p3, p4) = (p(1), p(2), p(3), p(4));
    let sql_upsert = match engine {
        Engine::Sqlite => super::album_metadata_repo::sql::upsert(&SqliteDialect),
        Engine::Postgres => super::album_metadata_repo::sql::upsert(&PostgresDialect),
    };
    let sql_compilation = |v: bool| match engine {
        Engine::Sqlite => super::album_repo::sql::set_compilation(&SqliteDialect, v),
        Engine::Postgres => super::album_repo::sql::set_compilation(&PostgresDialect, v),
    };
    let sql_lignes_album = sql_lignes(engine);
    let json_tenus = serde_json::to_string(&tenus.iter().collect::<Vec<_>>())
        .map_err(|e| RefusEdition::Base(e.to_string()))?;
    let nb_disques = disposition.len() as i64;

    db.write_tx(&mut |tx| {
        let id: &dyn ToSqlValue = &album_id;
        if let Some(t) = &titre {
            tx.execute(
                &format!("UPDATE albums SET title = {p1} WHERE id = {p2}"),
                &[t as &dyn ToSqlValue, id],
            )?;
        }
        if let Some(a) = artiste_album {
            tx.execute(
                &format!("UPDATE albums SET artist_id = {p1} WHERE id = {p2}"),
                &[&a as &dyn ToSqlValue, id],
            )?;
        }
        if let Some(y) = m.year {
            tx.execute(
                &format!("UPDATE albums SET year = {p1} WHERE id = {p2}"),
                &[&y as &dyn ToSqlValue, id],
            )?;
        }
        for (valeur, colonne) in [
            (&m.label, "label"),
            (&m.genre, "genre"),
            (&m.release_type, "release_type"),
        ] {
            if let Some(v) = valeur {
                let v = texte_libre(v);
                tx.execute(
                    &format!("UPDATE albums SET {colonne} = {p1} WHERE id = {p2}"),
                    &[&v as &dyn ToSqlValue, id],
                )?;
            }
        }
        for (numero_disque, nom, pistes) in &disposition {
            for (rang, piste) in pistes.iter().enumerate() {
                tx.execute(
                    &format!(
                        "UPDATE tracks SET disc_number = {p1}, track_number = {p2}, \
                         disc_subtitle = {p3} WHERE id = {p4}"
                    ),
                    &[
                        &(*numero_disque as i64) as &dyn ToSqlValue,
                        &((rang + 1) as i64),
                        nom,
                        piste,
                    ],
                )?;
            }
        }
        if nb_disques > 0 {
            tx.execute(
                &format!("UPDATE albums SET disc_count = {p1} WHERE id = {p2}"),
                &[&nb_disques as &dyn ToSqlValue, id],
            )?;
        }
        for (piste, t) in &titres_pistes {
            tx.execute(
                &format!("UPDATE tracks SET title = {p1} WHERE id = {p2}"),
                &[t as &dyn ToSqlValue, piste],
            )?;
        }
        for (piste, a) in &artistes_pistes {
            tx.execute(
                &format!("UPDATE tracks SET artist_id = {p1} WHERE id = {p2}"),
                &[a as &dyn ToSqlValue, piste],
            )?;
        }
        // Le mode de compilation — APRÈS les pistes : en `auto`, la règle
        // juge les artistes tels que cette édition vient de les poser.
        match mode.as_deref() {
            Some(MODE_OUI) => {
                tx.execute(&sql_compilation(true), &[id])?;
            }
            Some(MODE_NON) => {
                tx.execute(&sql_compilation(false), &[id])?;
            }
            Some(_) => {
                let mut indices = IndicesCompilation::new();
                for r in tx.query_many(
                    &format!(
                        "SELECT t.album_artist, ar.name FROM tracks t \
                         LEFT JOIN artists ar ON ar.id = t.artist_id WHERE t.album_id = {p1}"
                    ),
                    &[id],
                )? {
                    let balise = r.first().and_then(|v| v.as_string());
                    let artiste = r.get(1).and_then(|v| v.as_string());
                    indices.ajouter_piste(balise.as_deref(), artiste.as_deref());
                }
                tx.execute(&sql_compilation(indices.juger().compilation), &[id])?;
            }
            None => {}
        }
        if tenus != tenus_avant {
            tx.execute(
                &sql_upsert,
                &[id, &CLE_EDITION_MANUELLE as &dyn ToSqlValue, &json_tenus],
            )?;
        }
        if touche_pistes {
            let lignes = lignes_de(tx.query_many(&sql_lignes_album, &[id])?);
            let e = construire(
                &lignes,
                precedent.disposition || m.discs.is_some(),
                &[&precedent],
                &titres_pistes,
                &artistes_pistes,
            );
            let json = serde_json::to_string(&e).map_err(|x| x.to_string())?;
            tx.execute(
                &sql_upsert,
                &[id, &CLE_EDITION_PISTES as &dyn ToSqlValue, &json],
            )?;
        }
        Ok(())
    })?;
    if m.discs.is_some() {
        repo.update_track_count(album_id)?;
    }
    tracing::info!(
        album_id,
        champs = ?tenus.difference(&tenus_avant).collect::<Vec<_>>(),
        disques = nb_disques,
        pistes_renommees = titres_pistes.len() + artistes_pistes.len(),
        "album_edite_a_la_main"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Attacher / détacher un disque
// ---------------------------------------------------------------------------

fn numeros_de_disque(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Vec<i32>, TuneError> {
    let mut v: Vec<i32> =
        lignes_de(db.query_many_strong(&sql_lignes(db.engine()), &[&album_id as &dyn ToSqlValue])?)
            .into_iter()
            .map(|l| l.disque)
            .collect();
    v.sort_unstable();
    v.dedup();
    Ok(v)
}

fn poser_marqueur_coffret(
    db: &Arc<dyn DbBackend>,
    album_id: i64,
    m: &Marqueur,
) -> Result<(), TuneError> {
    let json = serde_json::to_string(m).map_err(|e| TuneError::from(e.to_string()))?;
    AlbumMetadataRepo::with_backend(db.clone()).set(album_id, CLE_COFFRET, &json)?;
    Ok(())
}

fn marqueur_coffret(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<Marqueur>, TuneError> {
    Ok(AlbumMetadataRepo::with_backend(db.clone())
        .get_all(album_id)?
        .get(CLE_COFFRET)
        .and_then(|v| serde_json::from_str::<Marqueur>(v).ok()))
}

/// `autre` devient le(s) disque(s) suivant(s) de `cible` : ses disques sont
/// renumérotés après le dernier de la cible, dans leur ordre, puis l'album est
/// absorbé ([`AlbumRepo::absorber`] : favoris, écoutes, étiquettes, dossiers
/// suivent). Le coffret est marqué `manuel`, et sa disposition tenue.
pub fn attacher(db: &Arc<dyn DbBackend>, cible: i64, autre: i64) -> Result<(), RefusEdition> {
    if cible == autre {
        return Err(invalide(
            "meme_album",
            "un album ne s'attache pas à lui-même",
        ));
    }
    let repo = AlbumRepo::with_backend(db.clone());
    if repo.get(cible)?.is_none() {
        return Err(RefusEdition::AlbumInconnu(cible));
    }
    if repo.get(autre)?.is_none() {
        return Err(RefusEdition::AlbumInconnu(autre));
    }
    let dernier = numeros_de_disque(db, cible)?.last().copied().unwrap_or(0);
    let siens = numeros_de_disque(db, autre)?;
    let heritage_cible = lire_edition(db, cible)?;
    let heritage_autre = lire_edition(db, autre)?;
    let p = |n| marque(db.engine(), n);
    let (p1, p2, p3) = (p(1), p(2), p(3));
    // Renuméroter d'abord vers des numéros NÉGATIFS provisoires, pour qu'un
    // disque 2 → 3 ne se mélange pas au disque 3 → 4 pendant la réécriture.
    for (rang, n) in siens.iter().enumerate() {
        let provisoire = -((rang + 1) as i64);
        db.execute(
            &format!(
                "UPDATE tracks SET disc_number = {p1} WHERE album_id = {p2} \
                 AND COALESCE(disc_number, 1) = {p3}"
            ),
            &[&provisoire as &dyn ToSqlValue, &autre, &(*n as i64)],
        )?;
    }
    for rang in 0..siens.len() {
        let provisoire = -((rang + 1) as i64);
        let final_ = (dernier as i64) + (rang as i64) + 1;
        db.execute(
            &format!(
                "UPDATE tracks SET disc_number = {p1} WHERE album_id = {p2} AND disc_number = {p3}"
            ),
            &[&final_ as &dyn ToSqlValue, &autre, &provisoire],
        )?;
    }
    // Ce qui est tenu sur l'album absorbé ne doit pas devenir, par la reprise
    // des clés d'`album_metadata`, tenu sur le coffret.
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    for cle in [CLE_EDITION_MANUELLE, CLE_EDITION_PISTES, CLE_COFFRET] {
        meta.delete(autre, cle)?;
    }
    repo.absorber(cible, autre)?;
    poser_marqueur_coffret(db, cible, &Marqueur::manuel())?;
    figer(db, cible, &[&heritage_cible, &heritage_autre])?;
    let disques = numeros_de_disque(db, cible)?.len() as i64;
    db.execute(
        &format!("UPDATE albums SET disc_count = {p1} WHERE id = {p2}"),
        &[&disques as &dyn ToSqlValue, &cible],
    )?;
    tracing::info!(cible, attache = autre, disques, "disque_attache");
    Ok(())
}

/// Le dossier commun des chemins donnés, s'il y en a un.
fn dossier_commun(chemins: &[&str]) -> Option<String> {
    let mut dossiers = chemins
        .iter()
        .filter_map(|c| c.rsplit_once('/').map(|(d, _)| d));
    let premier = dossiers.next()?;
    dossiers.all(|d| d == premier).then(|| premier.to_string())
}

/// Le disque `numero` de `album_id` redevient un album séparé. Rend son id.
///
/// Les pistes gardent leurs identifiants — donc leurs favoris, écoutes et
/// étiquettes. Le titre du nouvel album est celui du coffret, suivi du nom du
/// disque s'il en a un. Les disques restants sont renumérotés 1..n ; s'il
/// n'en reste qu'un, l'album n'est plus un coffret. Un coffret AUTOMATIQUE
/// est retenu comme refusé, exactement comme par « défaire » : la passe ne le
/// reformera pas.
pub fn detacher(db: &Arc<dyn DbBackend>, album_id: i64, numero: i32) -> Result<i64, RefusEdition> {
    let repo = AlbumRepo::with_backend(db.clone());
    let Some(coffret) = repo.get(album_id)? else {
        return Err(RefusEdition::AlbumInconnu(album_id));
    };
    let lignes =
        lignes_de(db.query_many_strong(&sql_lignes(db.engine()), &[&album_id as &dyn ToSqlValue])?);
    let numeros: BTreeSet<i32> = lignes.iter().map(|l| l.disque).collect();
    if !numeros.contains(&numero) {
        return Err(invalide(
            "disque_inconnu",
            format!("l'album {album_id} n'a pas de disque {numero}"),
        ));
    }
    if numeros.len() < 2 {
        return Err(invalide(
            "un_seul_disque",
            "l'album n'a qu'un disque : il n'y a rien à détacher",
        ));
    }
    let siennes: Vec<&Ligne> = lignes.iter().filter(|l| l.disque == numero).collect();
    let nom = siennes.iter().find_map(|l| l.nom_disque.clone());
    let heritage = lire_edition(db, album_id)?;
    let marqueur = marqueur_coffret(db, album_id)?;

    let mut disque = coffret.clone();
    disque.id = None;
    disque.title = match &nom {
        Some(n) => format!("{} — {n}", coffret.title),
        None => coffret.title.clone(),
    };
    disque.track_count = Some(0);
    disque.disc_count = None;
    disque.musicbrainz_release_id = None;
    let nouveau = repo.create(&disque)?;
    // Le dossier du disque, s'il en a un à lui : c'est l'identité qu'un scan
    // retrouvera. Un disque rangé dans le dossier du coffret n'en prend pas —
    // deux albums sur un dossier, et le scan n'en retrouverait qu'un.
    let chemins: Vec<&str> = siennes.iter().filter_map(|l| l.chemin.as_deref()).collect();
    if let Some(d) = dossier_commun(&chemins)
        && repo.folder_path_of(album_id)?.as_deref() != Some(d.as_str())
    {
        repo.set_folder_path(nouveau, &d)?;
    }
    let p = |n| marque(db.engine(), n);
    let (p1, p2, p3) = (p(1), p(2), p(3));
    db.execute(
        &format!(
            "UPDATE tracks SET album_id = {p1}, disc_number = 1 \
             WHERE album_id = {p2} AND COALESCE(disc_number, 1) = {p3}"
        ),
        &[&nouveau as &dyn ToSqlValue, &album_id, &(numero as i64)],
    )?;
    // Renuméroter les disques restants 1..n, dans leur ordre.
    for (rang, n) in numeros.iter().filter(|n| **n != numero).enumerate() {
        let final_ = (rang + 1) as i64;
        if final_ != *n as i64 {
            db.execute(
                &format!(
                    "UPDATE tracks SET disc_number = {p1} WHERE album_id = {p2} \
                     AND COALESCE(disc_number, 1) = {p3}"
                ),
                &[&final_ as &dyn ToSqlValue, &album_id, &(*n as i64)],
            )?;
        }
    }
    let restants = (numeros.len() - 1) as i64;
    db.execute(
        &format!("UPDATE albums SET disc_count = {p1} WHERE id = {p2}"),
        &[&restants as &dyn ToSqlValue, &album_id],
    )?;
    repo.update_track_count(album_id)?;
    repo.update_track_count(nouveau)?;

    // Le marqueur de coffret.
    if let Some(m) = &marqueur
        && m.origine == coffrets_auto::ORIGINE_AUTO
        && !m.cle.is_empty()
    {
        coffrets_auto::retenir_refus(db, &m.cle)?;
    }
    if restants < 2 {
        AlbumMetadataRepo::with_backend(db.clone()).delete(album_id, CLE_COFFRET)?;
    } else if marqueur.is_some() {
        poser_marqueur_coffret(db, album_id, &Marqueur::manuel())?;
    }
    figer(db, album_id, &[&heritage])?;
    figer(db, nouveau, &[&heritage])?;
    tracing::info!(
        coffret = album_id,
        disque = numero,
        nouvel_album = nouveau,
        "disque_detache"
    );
    Ok(nouveau)
}

#[cfg(test)]
#[path = "edition_album_tests.rs"]
pub(crate) mod tests;
