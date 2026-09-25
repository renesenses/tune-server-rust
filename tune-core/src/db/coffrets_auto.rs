//! Coffrets AUTOMATIQUES — GO de Bertrand du 25/09/2026 : « regroupement
//! automatique ».
//!
//! # Le fait, mesuré sur le .18 (9 430 albums)
//!
//! Des coffrets rangés un dossier par disque apparaissent comme des albums
//! séparés dont le titre porte le numéro de disque : *Early Works, Disc 1 /
//! Disc 2* (Laurent Garnier), *A Love Supreme, Disc 1 / Disc 2*, *Casino
//! Classics, Disc 1 / Disc 2*… La détection existait
//! ([`crate::metadata::coffrets::coffrets`]), mais il fallait réunir chaque
//! coffret À LA MAIN, un par un, depuis l'écran des coffrets éclatés.
//!
//! # Le modèle réutilisé
//!
//! Celui des coffrets manuels de la v0.9.161/162 : un coffret est UN album
//! qui a absorbé ses disques ([`AlbumRepo::absorber`]), dont chaque piste
//! porte son numéro de disque, et dont la fiche affiche un en-tête par disque.
//! Aucune table, aucune migration : ce qui doit durer vit dans les deux
//! magasins clé-valeur qui existent déjà sur les deux moteurs :
//!
//! - `album_metadata`, clé [`CLE_COFFRET`], sur l'album-coffret : QUI l'a
//!   composé (`auto` ou `manuel`) et, pour un coffret automatique, de quels
//!   disques il est fait — ce qu'il faut pour le défaire ;
//! - `settings`, clé [`CLE_REFUS`] : les coffrets que l'utilisateur a DÉFAITS,
//!   par leur identité stable (dossier parent + socle). Un rescan complet
//!   repart d'un `DELETE FROM albums` et emporte `album_metadata` ; un refus
//!   rangé là ne survivrait pas au premier rescan.
//!
//! # Ce que la passe ne touche JAMAIS
//!
//! 1. un coffret composé à la main (marqueur `manuel`) ;
//! 2. un album déjà réparti sur PLUSIEURS dossiers sans marqueur `auto` —
//!    coffret manuel antérieur au marqueur, coffret `CD01/CD02` du scan (C4) :
//!    on ne sait pas qui l'a fait, donc on n'y ajoute rien ;
//! 3. un coffret que l'utilisateur a défait ([`defaire`]) ;
//! 4. deux disques que l'utilisateur a déclarés distincts (#1276).
//!
//! Idempotente : un coffret réuni n'est plus qu'un album, sans frère à
//! absorber ; la passe suivante ne trouve rien.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::album_distinct_repo::{AlbumDistinctRepo, DistinctPairSet};
use super::album_metadata_repo::AlbumMetadataRepo;
use super::album_repo::AlbumRepo;
use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::settings_repo::SettingsRepo;
use crate::TuneError;
use crate::metadata::coffrets::{AlbumAGrouper, Coffret, coffrets};

/// Clé, dans `album_metadata`, du marqueur de coffret. Valeur : un
/// [`Marqueur`] en JSON.
pub const CLE_COFFRET: &str = "coffret";

/// Clé, dans `settings`, des coffrets défaits par l'utilisateur : tableau JSON
/// d'identités ([`Coffret::cle`]).
pub const CLE_REFUS: &str = "coffrets_auto_refuses";

/// Qui a composé le coffret.
pub const ORIGINE_AUTO: &str = "auto";
pub const ORIGINE_MANUEL: &str = "manuel";

/// Un disque d'un coffret automatique, tel qu'il était AVANT la réunion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisqueRetenu {
    pub n: u32,
    /// Le titre d'album d'origine (« Early Works, Disc 2 »).
    pub titre: String,
    /// Le dossier de ses pistes.
    pub dossier: String,
    pub artiste_id: Option<i64>,
}

/// Le marqueur posé sur l'album-coffret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marqueur {
    pub origine: String,
    /// Identité stable du coffret (vide pour un coffret manuel).
    #[serde(default)]
    pub cle: String,
    /// Les disques d'origine, par numéro croissant (vide pour un manuel).
    #[serde(default)]
    pub disques: Vec<DisqueRetenu>,
}

impl Marqueur {
    pub fn manuel() -> Self {
        Self {
            origine: ORIGINE_MANUEL.into(),
            cle: String::new(),
            disques: vec![],
        }
    }

    fn est_auto(&self) -> bool {
        self.origine == ORIGINE_AUTO
    }
}

/// Ce que la passe lit de la base, en trois requêtes.
pub struct Inventaire {
    pub albums: Vec<AlbumAGrouper>,
    /// Albums dont les pistes vivent dans PLUS D'UN dossier.
    pub plusieurs_dossiers: HashSet<i64>,
    pub marqueurs: HashMap<i64, Marqueur>,
}

/// Même liste que `is_various_artists` du scan (`tune-server`), qui n'est pas
/// visible d'ici.
fn artiste_de_compilation(nom: &str) -> bool {
    let l = nom.trim().to_lowercase();
    l == "various artists" || l == "various" || l == "va" || l == "compilations"
}

fn dossier_de(chemin: &str) -> Option<&str> {
    chemin.rsplit_once('/').map(|(d, _)| d)
}

fn placeholders(db: &Arc<dyn DbBackend>) -> (String, String) {
    match db.engine() {
        Engine::Postgres => (
            PostgresDialect.placeholder(1),
            PostgresDialect.placeholder(2),
        ),
        Engine::Sqlite => (SqliteDialect.placeholder(1), SqliteDialect.placeholder(2)),
    }
}

/// Un album, le PREMIER et le DERNIER chemin de ses pistes.
///
/// 🔴 `MIN(file_path)` : les disques d'un coffret n'ont qu'un dossier chacun,
/// et prendre le plus petit chemin rend un résultat STABLE d'un appel à
/// l'autre. `MAX` ne sert qu'à savoir si l'album tient dans un seul dossier.
const SQL_ALBUMS_ET_DOSSIERS: &str = "\
    SELECT t.album_id, al.title, MIN(t.file_path), MAX(t.file_path), al.artist_id, ar.name \
    FROM tracks t JOIN albums al ON al.id = t.album_id \
    LEFT JOIN artists ar ON ar.id = al.artist_id \
    WHERE t.file_path IS NOT NULL AND t.file_path <> '' \
    GROUP BY t.album_id, al.title, al.artist_id, ar.name";

pub fn marqueurs(db: &Arc<dyn DbBackend>) -> Result<HashMap<i64, Marqueur>, TuneError> {
    let (p1, _) = placeholders(db);
    Ok(db
        .query_many(
            &format!("SELECT album_id, value FROM album_metadata WHERE key = {p1}"),
            &[&CLE_COFFRET as &dyn ToSqlValue],
        )?
        .into_iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let m = serde_json::from_str::<Marqueur>(&r.get(1)?.as_string()?).ok()?;
            Some((id, m))
        })
        .collect())
}

pub fn inventaire(db: &Arc<dyn DbBackend>) -> Result<Inventaire, TuneError> {
    let mut albums = Vec::new();
    let mut plusieurs_dossiers = HashSet::new();
    for r in db.query_many(SQL_ALBUMS_ET_DOSSIERS, &[])? {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let Some(premier) = r.get(2).and_then(|v| v.as_string()) else {
            continue;
        };
        let dernier = r.get(3).and_then(|v| v.as_string()).unwrap_or_default();
        let Some(dossier) = dossier_de(&premier) else {
            continue;
        };
        if dossier_de(&dernier) != Some(dossier) {
            plusieurs_dossiers.insert(id);
        }
        albums.push(AlbumAGrouper {
            id,
            titre: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            dossier: dossier.to_string(),
            artiste_id: r.get(4).and_then(|v| v.as_i64()),
            artiste_de_compilation: r
                .get(5)
                .and_then(|v| v.as_string())
                .is_some_and(|n| artiste_de_compilation(&n)),
        });
    }
    Ok(Inventaire {
        albums,
        plusieurs_dossiers,
        marqueurs: marqueurs(db)?,
    })
}

// ---------------------------------------------------------------------------
// Les refus — coffrets DÉFAITS par l'utilisateur
// ---------------------------------------------------------------------------

pub fn refus(db: &Arc<dyn DbBackend>) -> BTreeSet<String> {
    SettingsRepo::with_backend(db.clone())
        .get(CLE_REFUS)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<BTreeSet<String>>(&v).ok())
        .unwrap_or_default()
}

fn ecrire_refus(db: &Arc<dyn DbBackend>, r: &BTreeSet<String>) -> Result<(), TuneError> {
    let json = serde_json::to_string(r).map_err(|e| e.to_string())?;
    SettingsRepo::with_backend(db.clone()).set(CLE_REFUS, &json)?;
    Ok(())
}

/// L'utilisateur REVIENT sur un refus en réunissant lui-même le coffret.
pub fn oublier_refus(db: &Arc<dyn DbBackend>, cle: &str) -> Result<(), TuneError> {
    let mut r = refus(db);
    if r.remove(cle) {
        ecrire_refus(db, &r)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Réunir
// ---------------------------------------------------------------------------

/// Réunit UN coffret dans son disque de plus petit numéro, et le marque
/// `auto`. Rend le nombre de disques absorbés.
///
/// Le numéro de disque des pistes est réécrit d'après le marqueur — comme le
/// fait la composition manuelle d'après l'ordre choisi — sauf pour un album
/// déjà réparti sur plusieurs dossiers, qui porte déjà les siens (un coffret
/// réuni auquel s'ajoute un disque arrivé plus tard). Rien n'est écrit dans
/// les FICHIERS.
pub fn reunir(db: &Arc<dyn DbBackend>, c: &Coffret, inv: &Inventaire) -> Result<usize, TuneError> {
    let cible = c
        .cible()
        .ok_or_else(|| TuneError::from("coffret sans disque".to_string()))?;
    let (p1, p2) = placeholders(db);
    let par_id: HashMap<i64, &AlbumAGrouper> = inv.albums.iter().map(|a| (a.id, a)).collect();

    // Les disques d'origine, pour pouvoir DÉFAIRE : ceux d'un membre déjà
    // réuni viennent de son propre marqueur.
    let mut disques: Vec<DisqueRetenu> = Vec::new();
    for &(n, id) in &c.disques {
        match inv.marqueurs.get(&id).filter(|m| m.est_auto()) {
            Some(m) if !m.disques.is_empty() => disques.extend(m.disques.iter().cloned()),
            _ => {
                if let Some(a) = par_id.get(&id) {
                    disques.push(DisqueRetenu {
                        n,
                        titre: a.titre.clone(),
                        dossier: a.dossier.clone(),
                        artiste_id: a.artiste_id,
                    });
                }
            }
        }
    }
    disques.sort_by_key(|d| d.n);
    disques.dedup_by(|a, b| a.dossier == b.dossier);

    for &(n, id) in &c.disques {
        if inv.plusieurs_dossiers.contains(&id) {
            continue;
        }
        db.execute(
            &format!("UPDATE tracks SET disc_number = {p1} WHERE album_id = {p2}"),
            &[&(n as i64) as &dyn ToSqlValue, &id],
        )?;
    }
    let repo = AlbumRepo::with_backend(db.clone());
    let mut absorbes = 0usize;
    for id in c.absorbes() {
        repo.absorber(cible, id)?;
        absorbes += 1;
    }
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    // Un titre corrigé à la main (C3) reste celui de l'utilisateur.
    let titre_tenu = meta
        .champs_edites_a_la_main(cible)
        .unwrap_or_default()
        .iter()
        .any(|ch| ch == "title");
    // ⚠️ Au-delà de ce point, les pistes SONT réunies : un titre ou un
    // marqueur non écrit n'annule rien, et refuser laisserait la bibliothèque
    // à mi-chemin (même choix que la route d'avant). Sans marqueur, l'album
    // réparti sur plusieurs dossiers est ensuite épargné par la passe, comme
    // tout coffret dont on ignore l'auteur.
    if !titre_tenu && let Err(e) = repo.force_update_title(cible, &c.titre) {
        tracing::warn!(album = cible, erreur = %e, "coffret_titre_non_renomme");
    }
    let marqueur = Marqueur {
        origine: ORIGINE_AUTO.into(),
        cle: c.cle.clone(),
        disques,
    };
    let json = serde_json::to_string(&marqueur).unwrap_or_default();
    if let Err(e) = meta.set(cible, CLE_COFFRET, &json) {
        tracing::warn!(album = cible, erreur = %e, "coffret_non_marque");
    }
    Ok(absorbes)
}

/// Ce que la passe a fait — et ce qu'elle a laissé, par raison.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RapportPasse {
    pub reunis: usize,
    pub disques_absorbes: usize,
    pub laisses_refuses: usize,
    pub laisses_distincts: usize,
    pub laisses_manuels: usize,
    pub echecs: usize,
}

impl RapportPasse {
    pub fn rien_a_dire(&self) -> bool {
        *self == Self::default()
    }
}

fn une_paire_distincte(c: &Coffret, d: &DistinctPairSet) -> bool {
    c.disques
        .iter()
        .enumerate()
        .any(|(i, (_, a))| c.disques[i + 1..].iter().any(|(_, b)| d.contains(*a, *b)))
}

/// Un membre que la passe ne doit pas toucher : un coffret composé à la main,
/// ou un album réparti sur plusieurs dossiers dont on ne sait pas qui l'a fait.
fn tenu_a_la_main(id: i64, inv: &Inventaire) -> bool {
    match inv.marqueurs.get(&id) {
        Some(m) => !m.est_auto(),
        None => inv.plusieurs_dossiers.contains(&id),
    }
}

/// LA PASSE — au démarrage et après chaque scan. Ne relit aucun fichier.
pub fn passe(db: &Arc<dyn DbBackend>) -> Result<RapportPasse, TuneError> {
    let inv = inventaire(db)?;
    let refuses = refus(db);
    // Un échec de lecture rend l'ensemble VIDE — même choix que le
    // rapprochement des doublons (`paires_distinctes`).
    let distinctes = AlbumDistinctRepo::with_backend(db.clone())
        .charger_ensemble()
        .unwrap_or_default();
    let mut r = RapportPasse::default();
    for c in coffrets(&inv.albums) {
        if refuses.contains(&c.cle) {
            r.laisses_refuses += 1;
            continue;
        }
        if une_paire_distincte(&c, &distinctes) {
            r.laisses_distincts += 1;
            continue;
        }
        if c.disques.iter().any(|(_, id)| tenu_a_la_main(*id, &inv)) {
            r.laisses_manuels += 1;
            continue;
        }
        match reunir(db, &c, &inv) {
            Ok(n) => {
                r.reunis += 1;
                r.disques_absorbes += n;
                tracing::info!(cible = ?c.cible(), disques = c.disques.len(), titre = %c.titre, "coffret_auto_reuni");
            }
            Err(e) => {
                r.echecs += 1;
                tracing::warn!(cible = ?c.cible(), erreur = %e, "coffret_auto_echec");
            }
        }
    }
    Ok(r)
}

/// La passe, journalisée — la forme qu'appellent le démarrage et les scans.
pub fn passe_journalisee(db: &Arc<dyn DbBackend>, moment: &'static str) -> RapportPasse {
    match passe(db) {
        Ok(r) => {
            if !r.rien_a_dire() {
                tracing::info!(
                    moment,
                    reunis = r.reunis,
                    disques_absorbes = r.disques_absorbes,
                    laisses_refuses = r.laisses_refuses,
                    laisses_distincts = r.laisses_distincts,
                    laisses_manuels = r.laisses_manuels,
                    echecs = r.echecs,
                    "coffrets_auto_passe"
                );
            }
            r
        }
        Err(e) => {
            tracing::warn!(moment, erreur = %e, "coffrets_auto_passe_echec");
            RapportPasse::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Défaire
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum RefusDefaire {
    /// L'album n'existe pas, ou n'est pas un coffret automatique.
    PasUnCoffretAuto,
    Base(String),
}

impl From<TuneError> for RefusDefaire {
    fn from(e: TuneError) -> Self {
        Self::Base(e.to_string())
    }
}

impl From<String> for RefusDefaire {
    fn from(e: String) -> Self {
        Self::Base(e)
    }
}

/// DÉFAIT un coffret automatique : chaque disque redevient un album, sous son
/// titre d'origine, et le coffret est RETENU comme refusé — la passe
/// suivante ne le reforme pas, pas même après un rescan complet.
///
/// Rend les identifiants des albums recréés.
pub fn defaire(db: &Arc<dyn DbBackend>, cible: i64) -> Result<Vec<i64>, RefusDefaire> {
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    let marqueur = meta
        .get_all(cible)?
        .get(CLE_COFFRET)
        .and_then(|v| serde_json::from_str::<Marqueur>(v).ok())
        .filter(|m| m.est_auto() && !m.disques.is_empty())
        .ok_or(RefusDefaire::PasUnCoffretAuto)?;
    let repo = AlbumRepo::with_backend(db.clone());
    let album = repo.get(cible)?.ok_or(RefusDefaire::PasUnCoffretAuto)?;
    let (p1, _) = placeholders(db);
    let pistes: Vec<(i64, String)> = db
        .query_many(
            &format!("SELECT id, file_path FROM tracks WHERE album_id = {p1}"),
            &[&cible as &dyn ToSqlValue],
        )?
        .into_iter()
        .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_string()?)))
        .collect();
    let (p1, p2) = placeholders(db);
    let mut recrees = Vec::new();
    for d in marqueur.disques.iter().skip(1) {
        let prefixe = format!("{}/", d.dossier);
        let siennes: Vec<i64> = pistes
            .iter()
            .filter(|(_, chemin)| chemin.starts_with(&prefixe))
            .map(|(id, _)| *id)
            .collect();
        if siennes.is_empty() {
            continue;
        }
        let mut disque = album.clone();
        disque.id = None;
        disque.title = d.titre.clone();
        disque.artist_id = d.artiste_id.or(album.artist_id);
        disque.track_count = Some(0);
        disque.disc_count = None;
        disque.musicbrainz_release_id = None;
        let id = repo.create(&disque)?;
        repo.set_folder_path(id, &d.dossier)?;
        for piste in siennes {
            db.execute(
                &format!("UPDATE tracks SET album_id = {p1} WHERE id = {p2}"),
                &[&id as &dyn ToSqlValue, &piste],
            )?;
        }
        repo.update_track_count(id)?;
        recrees.push(id);
    }
    let titre_tenu = meta
        .champs_edites_a_la_main(cible)
        .unwrap_or_default()
        .iter()
        .any(|ch| ch == "title");
    if !titre_tenu && let Some(premier) = marqueur.disques.first() {
        repo.force_update_title(cible, &premier.titre)?;
    }
    repo.update_track_count(cible)?;
    meta.delete(cible, CLE_COFFRET)?;
    let mut r = refus(db);
    r.insert(marqueur.cle.clone());
    ecrire_refus(db, &r)?;
    Ok(recrees)
}

// ---------------------------------------------------------------------------
// Lister — l'onglet « Coffrets » de la Bibliothèque
// ---------------------------------------------------------------------------

/// Un coffret de la bibliothèque, pour l'onglet « Coffrets ».
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoffretListe {
    pub album_id: i64,
    /// `auto`, `manuel`, ou `None` : un coffret sans marqueur (composé avant
    /// le marqueur, ou rangé en `CD01/CD02` et réuni par le scan).
    pub origine: Option<String>,
    pub disques: i64,
}

/// Les coffrets : tout album MARQUÉ, plus tout album dont les pistes portent
/// au moins deux numéros de disque ET vivent dans au moins deux dossiers.
///
/// ⚠️ Le second critère exclut à dessein le double album rangé dans UN
/// dossier (pistes 1-01 à 2-12) : c'est un album, pas un coffret rangé disque
/// par disque. Les albums masqués (#1391) n'y figurent pas.
pub fn lister(db: &Arc<dyn DbBackend>) -> Result<Vec<CoffretListe>, TuneError> {
    let sql = format!(
        "SELECT t.album_id, COUNT(DISTINCT t.disc_number), MIN(t.file_path), MAX(t.file_path) \
         FROM tracks t JOIN albums a ON a.id = t.album_id \
         WHERE t.file_path IS NOT NULL AND t.file_path <> '' AND {} \
         GROUP BY t.album_id",
        super::facet_filter::hidden_albums_excluded()
    );
    let marques = marqueurs(db)?;
    let mut rendu = Vec::new();
    for r in db.query_many(&sql, &[])? {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let disques = r.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
        let premier = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        let dernier = r.get(3).and_then(|v| v.as_string()).unwrap_or_default();
        let origine = marques.get(&id).map(|m| m.origine.clone());
        let range_disque_par_disque = disques >= 2 && dossier_de(&premier) != dossier_de(&dernier);
        if origine.is_some() || range_disque_par_disque {
            rendu.push(CoffretListe {
                album_id: id,
                origine,
                disques,
            });
        }
    }
    Ok(rendu)
}

#[cfg(test)]
#[path = "coffrets_auto_tests.rs"]
pub(crate) mod tests;
