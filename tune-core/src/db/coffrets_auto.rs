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
//! 4. deux disques que l'utilisateur a déclarés distincts (#1276) ;
//! 5. un album dont l'utilisateur a disposé les disques à la main (écran
//!    « Modifier » de la fiche, [`super::edition_album`]).
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
use super::track_repo::sql::chemin_ouvrable;
use crate::TuneError;
use crate::library::local_path::{dossier_comparable, dossier_et_nom, sous_le_dossier_stocke};
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
    /// Les disques d'origine, par numéro croissant. Pour un coffret manuel,
    /// ceux que la composition a réunis (vide pour un coffret composé avant
    /// #5319, ou par « attacher »).
    #[serde(default)]
    pub disques: Vec<DisqueRetenu>,
    /// Coffret manuel : le titre que la COMPOSITION lui a donné (#5319).
    /// « Défaire » ne rend son titre d'origine à l'album que s'il porte
    /// encore celui-là — un titre changé depuis est celui de l'utilisateur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub titre_compose: Option<String>,
    /// Coffret manuel : le titre de l'album cible était DÉJÀ tenu à la main
    /// (`edition_manuelle`) avant la composition. « Défaire » ne retire
    /// alors pas ce marquage, qui n'est pas le sien.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub titre_tenu_avant: bool,
    /// Fil 2094 — les numéros des disques dont la composition a fait du
    /// titre d'origine le SOUS-TITRE ([`poser_les_sous_titres`]). « Défaire »
    /// ne retire que ceux-là ([`retirer_les_sous_titres`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sous_titres: Vec<u32>,
}

impl Marqueur {
    pub fn manuel() -> Self {
        Self {
            origine: ORIGINE_MANUEL.into(),
            cle: String::new(),
            disques: vec![],
            titre_compose: None,
            titre_tenu_avant: false,
            sous_titres: vec![],
        }
    }

    pub fn est_manuel(&self) -> bool {
        self.origine == ORIGINE_MANUEL
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
    /// Albums dont l'utilisateur a DISPOSÉ les disques à la main (écran
    /// « Modifier » de la fiche, GO du 25/09/2026) : la passe n'y touche pas.
    pub disposes: HashSet<i64>,
}

/// Même liste que `is_various_artists` du scan (`tune-server`), qui n'est pas
/// visible d'ici.
fn artiste_de_compilation(nom: &str) -> bool {
    let l = nom.trim().to_lowercase();
    l == "various artists" || l == "various" || l == "va" || l == "compilations"
}

/// Le dossier d'un chemin stocké, `/` et `\` confondus sous Windows (#5318).
///
/// 🔴 `rsplit_once('/')` seul ne trouvait RIEN dans `D:\Musique\X\CD1\01.flac`
/// — `tracks.file_path` porte des antislashs sous Windows : la passe sautait
/// tous les albums, et l'onglet « Coffrets » n'en voyait aucun rangé disque
/// par disque. La coupe est celle de [`dossier_et_nom`], qui s'appuie sur la
/// reconnaissance des racines de `library::local_path` (lecteur, UNC, POSIX).
fn dossier_de(chemin: &str) -> Option<&str> {
    dossier_et_nom(chemin).map(|(d, _)| d)
}

/// Deux dossiers stockés désignent-ils le même ? Casse du lecteur et
/// séparateurs Windows ne comptent pas ([`dossier_comparable`]).
fn meme_dossier(a: Option<&str>, b: Option<&str>) -> bool {
    a.map(dossier_comparable) == b.map(dossier_comparable)
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
/// 🔴 `MIN(chemin)` : les disques d'un coffret n'ont qu'un dossier chacun,
/// et prendre le plus petit chemin rend un résultat STABLE d'un appel à
/// l'autre. `MAX` ne sert qu'à savoir si l'album tient dans un seul dossier.
///
/// 🔴 `COALESCE(t.source, 'local') = 'local'` — et pas le chemin de fichier
/// seul. Le chemin n'est pas un substitut de la source : rien n'interdit à une
/// source distante d'en porter un, et cet inventaire alimente à la fois
/// l'écran des coffrets éclatés, le geste de regroupement et la passe
/// automatique ([`passe`]), qui RÉUNIT des albums. Voir
/// [`tune_core::db::track_repo::sql::est_local`] pour le choix de la forme.
///
/// 🔴 Le chemin est [`chemin_ouvrable`] — le fichier, ou pour une tranche
/// découpée par une feuille CUE, l'IMAGE qui la porte (#5317). `t.file_path`
/// seul NE SUFFIT PAS : une piste CUE a `file_path = NULL` par construction
/// (`scanner/cue_bibliotheque.rs`, « une piste CUE n'a pas de file_path »).
/// Filtrer sur lui écartait de la passe TOUT album né d'une feuille : le
/// *Messiah* de Gardiner en APE+CUE, deux disques sous `…/CD1` et `…/CD2`,
/// n'était jamais examiné. L'image vit dans le dossier du disque : son
/// dossier est celui du disque, comme pour un fichier.
fn sql_albums_et_dossiers() -> String {
    format!(
        "SELECT t.album_id, al.title, MIN({c}), MAX({c}), al.artist_id, ar.name \
         FROM tracks t JOIN albums al ON al.id = t.album_id \
         LEFT JOIN artists ar ON ar.id = al.artist_id \
         WHERE {piste_locale} AND {c} IS NOT NULL \
         GROUP BY t.album_id, al.title, al.artist_id, ar.name",
        piste_locale = crate::db::track_repo::sql::PISTE_LOCALE,
        c = chemin_ouvrable!(),
    )
}

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
    for r in db.query_many(&sql_albums_et_dossiers(), &[])? {
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
        if !meme_dossier(dossier_de(&dernier), Some(dossier)) {
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
        disposes: super::edition_album::Tenues::charger(db)
            .albums_disposes()
            .clone(),
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

/// Retient un coffret comme REFUSÉ — ce que fait [`defaire`], et le geste
/// « détacher un disque » d'un coffret automatique.
pub fn retenir_refus(db: &Arc<dyn DbBackend>, cle: &str) -> Result<(), TuneError> {
    let mut r = refus(db);
    if r.insert(cle.to_string()) {
        ecrire_refus(db, &r)?;
    }
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
    disques.dedup_by(|a, b| meme_dossier(Some(&a.dossier), Some(&b.dossier)));

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
    // Fil 2094 — le titre d'origine de chaque disque devient son sous-titre,
    // comparé au titre que le coffret porte VRAIMENT (celui de l'utilisateur
    // s'il le tient à la main).
    let titre_du_coffret = repo
        .get(cible)
        .ok()
        .flatten()
        .map(|a| a.title)
        .unwrap_or_else(|| c.titre.clone());
    let mut sous_titres = poser_les_sous_titres(db, cible, &disques, &titre_du_coffret)
        .unwrap_or_else(|e| {
            tracing::warn!(album = cible, erreur = %e, "coffret_sous_titres_non_poses");
            Vec::new()
        });
    // Un coffret déjà réuni auquel s'ajoute un disque arrivé plus tard : ses
    // disques portent DÉJÀ les sous-titres de la première réunion, que
    // `poser_les_sous_titres` ne repose donc pas. Sans les reprendre de son
    // marqueur, « Défaire » ne les retirerait plus.
    for &(_, id) in &c.disques {
        if let Some(m) = inv.marqueurs.get(&id).filter(|m| m.est_auto()) {
            sous_titres.extend(m.sous_titres.iter().copied());
        }
    }
    sous_titres.sort_unstable();
    sous_titres.dedup();
    let marqueur = Marqueur {
        origine: ORIGINE_AUTO.into(),
        cle: c.cle.clone(),
        disques,
        titre_compose: None,
        titre_tenu_avant: false,
        sous_titres,
    };
    let json = serde_json::to_string(&marqueur).unwrap_or_default();
    if let Err(e) = meta.set(cible, CLE_COFFRET, &json) {
        tracing::warn!(album = cible, erreur = %e, "coffret_non_marque");
    }
    Ok(absorbes)
}

/// Deux titres qui disent la même chose : casse, accents et espaces ignorés.
fn meme_titre(a: &str, b: &str) -> bool {
    let cle = |s: &str| {
        crate::db::engine::fold_diacritics(s)
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    cle(a) == cle(b)
}

/// Fil 2094 (décision de Bertrand du 05/10/2026) — le titre d'ORIGINE de
/// chaque disque devient son sous-titre de disque (`tracks.disc_subtitle`),
/// sauf s'il est identique au titre du coffret.
///
/// La composition, manuelle ou automatique, ne gardait ces titres que dans le
/// marqueur : la fiche affichait « Disque 1 », « Disque 2 »… là où les albums
/// réunis s'appelaient « 101 - Disc A », « Bach : Cantates, vol. 3 ».
///
/// Appelée APRÈS la réunion : les pistes sont toutes dans `cible`, chaque
/// disque sous son numéro `n`. Un sous-titre déjà présent (balise
/// DISCSUBTITLE, nom donné à la main) n'est jamais remplacé, ni complété sur
/// les autres pistes de son disque. Rend les numéros
/// des disques effectivement sous-titrés, à retenir dans le marqueur.
pub fn poser_les_sous_titres(
    db: &Arc<dyn DbBackend>,
    cible: i64,
    disques: &[DisqueRetenu],
    titre_du_coffret: &str,
) -> Result<Vec<u32>, TuneError> {
    let p = |i| match db.engine() {
        Engine::Postgres => PostgresDialect.placeholder(i),
        Engine::Sqlite => SqliteDialect.placeholder(i),
    };
    // Un disque dont UNE piste porte déjà un nom est laissé entier : un
    // sous-titre posé sur ses autres pistes le couperait en deux en-têtes.
    let sql = format!(
        "UPDATE tracks SET disc_subtitle = {} WHERE album_id = {} \
         AND COALESCE(disc_number, 1) = {} \
         AND NOT EXISTS (SELECT 1 FROM tracks n WHERE n.album_id = {} \
         AND COALESCE(n.disc_number, 1) = {} AND TRIM(COALESCE(n.disc_subtitle, '')) <> '')",
        p(1),
        p(2),
        p(3),
        p(4),
        p(5)
    );
    let mut poses = Vec::new();
    for d in disques {
        let titre = d.titre.trim();
        if titre.is_empty() || meme_titre(titre, titre_du_coffret) {
            continue;
        }
        let numero = d.n as i64;
        let n = db.execute(
            &sql,
            &[
                &titre.to_string() as &dyn ToSqlValue,
                &cible,
                &numero,
                &cible,
                &numero,
            ],
        )?;
        if n > 0 {
            poses.push(d.n);
        }
    }
    Ok(poses)
}

/// « Défaire » un coffret : retire les sous-titres que la composition avait
/// posés ([`poser_les_sous_titres`]), sur les albums rendus. Seulement ceux
/// que le marqueur retient, et seulement s'ils portent encore le titre
/// d'origine — un nom changé depuis est celui de l'utilisateur.
pub fn retirer_les_sous_titres(
    db: &Arc<dyn DbBackend>,
    albums: &[i64],
    marqueur: &Marqueur,
) -> Result<(), TuneError> {
    if albums.is_empty() {
        return Ok(());
    }
    let liste = albums
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let (p1, _) = placeholders(db);
    let sql = format!(
        "UPDATE tracks SET disc_subtitle = NULL WHERE album_id IN ({liste}) \
         AND disc_subtitle = {p1}"
    );
    for d in marqueur
        .disques
        .iter()
        .filter(|d| marqueur.sous_titres.contains(&d.n))
    {
        db.execute(&sql, &[&d.titre.trim().to_string() as &dyn ToSqlValue])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rattrapage des coffrets composés AVANT le fil 2094
// ---------------------------------------------------------------------------

/// Clé de `settings` qui marque le rattrapage des sous-titres comme fait : il
/// ne se rejoue pas à chaque démarrage (même modèle que
/// `reparation_file_first_seen_5389`).
pub const CLE_RATTRAPAGE_SOUS_TITRES_2094: &str = "rattrapage_sous_titres_coffrets_2094";

/// Ce que le rattrapage a fait — et ce qu'il a laissé, par raison.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RattrapageSousTitres {
    /// Le marqueur de `settings` était déjà posé : rien n'a été lu ni écrit.
    pub deja_fait: bool,
    /// Coffrets examinés (tout album qui porte un marqueur `coffret`).
    pub coffrets: usize,
    /// Coffrets dont au moins un disque a reçu son sous-titre.
    pub coffrets_rattrapes: usize,
    /// Disques sous-titrés par ce rattrapage.
    pub disques_sous_titres: usize,
    /// Coffrets dont le marqueur ne retient AUCUN titre d'origine : coffret
    /// manuel composé avant #5319, ou formé par « attacher ». Rien à poser.
    pub titres_inconnus: usize,
    /// Disques laissés parce que leurs pistes ne sont plus dans le dossier
    /// que le marqueur retient : disques renumérotés ou déplacés depuis la
    /// composition, le titre d'origine ne désigne plus sûrement ce disque.
    pub disques_deplaces: usize,
    /// Coffrets sur lesquels une écriture a échoué.
    pub echecs: usize,
}

/// Fil 2094 — les coffrets composés AVANT que la composition pose les
/// sous-titres de disque ([`poser_les_sous_titres`]) n'en ont aucun : une
/// passe UNIQUE leur applique la même règle.
///
/// Pour chaque album marqué `coffret`, automatique ou manuel :
/// - le titre d'origine de chaque disque, lu dans le marqueur, devient son
///   sous-titre — pas s'il est identique au titre que porte le coffret, jamais
///   sur un disque qui porte déjà un nom ;
/// - les numéros sous-titrés rejoignent `Marqueur::sous_titres`, pour que
///   « Défaire » retire exactement ceux-là ;
/// - si la disposition de l'album est TENUE (coffret manuel, ou disposé à la
///   main), elle est retenue à nouveau, nom de disque compris : sans cela, la
///   relecture des fichiers rendrait au disque le nom (vide) retenu avant.
///
/// Un disque dont les pistes ne vivent plus dans le dossier que le marqueur
/// retient est laissé : renuméroté depuis, son numéro ne désigne plus le
/// disque d'origine. Un coffret dont le marqueur ne retient aucun disque est
/// laissé aussi : son titre d'origine a disparu avec l'album absorbé.
///
/// Aucun fichier n'est lu. Le marqueur de `settings` n'est posé que si aucun
/// coffret n'a échoué : sinon le rattrapage repasse au démarrage suivant, sans
/// risque, puisqu'un disque déjà nommé n'est jamais renommé.
pub fn rattraper_les_sous_titres(
    db: &Arc<dyn DbBackend>,
) -> Result<RattrapageSousTitres, TuneError> {
    let reglages = SettingsRepo::with_backend(db.clone());
    if reglages.get(CLE_RATTRAPAGE_SOUS_TITRES_2094)?.is_some() {
        return Ok(RattrapageSousTitres {
            deja_fait: true,
            ..Default::default()
        });
    }
    let mut tous: Vec<(i64, Marqueur)> = marqueurs(db)?.into_iter().collect();
    tous.sort_by_key(|(id, _)| *id);
    let mut r = RattrapageSousTitres::default();
    for (id, mut m) in tous {
        r.coffrets += 1;
        if m.disques.is_empty() {
            r.titres_inconnus += 1;
            continue;
        }
        match rattraper_un_coffret(db, id, &mut m) {
            Ok((poses, deplaces)) => {
                r.disques_deplaces += deplaces;
                if poses > 0 {
                    r.coffrets_rattrapes += 1;
                    r.disques_sous_titres += poses;
                }
            }
            Err(e) => {
                tracing::warn!(album = id, erreur = %e, "coffret_sous_titres_rattrapage_echoue");
                r.echecs += 1;
            }
        }
    }
    if r.echecs == 0 {
        reglages.set(CLE_RATTRAPAGE_SOUS_TITRES_2094, "1")?;
    }
    Ok(r)
}

/// Un coffret : rend `(disques sous-titrés, disques laissés car déplacés)`.
fn rattraper_un_coffret(
    db: &Arc<dyn DbBackend>,
    id: i64,
    m: &mut Marqueur,
) -> Result<(usize, usize), TuneError> {
    let Some(album) = AlbumRepo::with_backend(db.clone()).get(id)? else {
        return Ok((0, 0));
    };
    let mut retenus = Vec::new();
    let mut deplaces = 0usize;
    for d in &m.disques {
        if m.sous_titres.contains(&d.n) {
            continue;
        }
        if disque_a_sa_place(db, id, d)? {
            retenus.push(d.clone());
        } else {
            deplaces += 1;
        }
    }
    let poses = poser_les_sous_titres(db, id, &retenus, &album.title)?;
    if poses.is_empty() {
        return Ok((0, deplaces));
    }
    m.sous_titres.extend(poses.iter().copied());
    m.sous_titres.sort_unstable();
    m.sous_titres.dedup();
    let json = serde_json::to_string(m).map_err(|e| TuneError::from(e.to_string()))?;
    AlbumMetadataRepo::with_backend(db.clone()).set(id, CLE_COFFRET, &json)?;
    super::edition_album::retenir_les_noms_de_disque(db, id)?;
    Ok((poses.len(), deplaces))
}

/// Les pistes du disque `d.n` de l'album vivent-elles toutes dans le dossier
/// que le marqueur retient pour ce disque ? Vrai sans dossier retenu, ou sans
/// piste à chemin (rien ne contredit alors le marqueur).
fn disque_a_sa_place(
    db: &Arc<dyn DbBackend>,
    album: i64,
    d: &DisqueRetenu,
) -> Result<bool, TuneError> {
    if d.dossier.trim().is_empty() {
        return Ok(true);
    }
    let (p1, p2) = placeholders(db);
    let sql = format!(
        "SELECT DISTINCT {c} FROM tracks t WHERE t.album_id = {p1} \
         AND COALESCE(t.disc_number, 1) = {p2} AND {c} IS NOT NULL",
        c = chemin_ouvrable!(),
    );
    let rows = db.query_many(&sql, &[&album as &dyn ToSqlValue, &(d.n as i64)])?;
    Ok(rows
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .all(|chemin| meme_dossier(dossier_de(&chemin), Some(&d.dossier))))
}

/// Le rattrapage, journalisé — la forme qu'appelle le démarrage. Une erreur
/// se journalise et ne remonte pas : elle ne bloque jamais le démarrage.
pub fn rattrapage_journalise(db: &Arc<dyn DbBackend>) -> RattrapageSousTitres {
    match rattraper_les_sous_titres(db) {
        Ok(r) => {
            if !r.deja_fait {
                tracing::info!(
                    coffrets = r.coffrets,
                    coffrets_rattrapes = r.coffrets_rattrapes,
                    disques_sous_titres = r.disques_sous_titres,
                    titres_inconnus = r.titres_inconnus,
                    disques_deplaces = r.disques_deplaces,
                    echecs = r.echecs,
                    "coffrets_sous_titres_rattrapage_2094"
                );
            }
            r
        }
        Err(e) => {
            tracing::warn!(erreur = %e, "coffrets_sous_titres_rattrapage_2094_echec");
            RattrapageSousTitres::default()
        }
    }
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
    if inv.disposes.contains(&id) {
        return true;
    }
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
            // Le même chemin que l'inventaire : une piste CUE n'a pas de
            // `file_path`, et la filtrer ici laisserait son disque DANS le
            // coffret tout en retenant le refus (#5317).
            &format!(
                "SELECT t.id, {} FROM tracks t WHERE t.album_id = {p1}",
                chemin_ouvrable!()
            ),
            &[&cible as &dyn ToSqlValue],
        )?
        .into_iter()
        .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_string()?)))
        .collect();
    let (p1, p2) = placeholders(db);
    let mut recrees = Vec::new();
    for d in marqueur.disques.iter().skip(1) {
        // `…/CD2/` en dur ne reconnaissait aucune piste de `C:\…\CD2\` : sous
        // Windows le disque restait DANS le coffret alors que le refus était
        // retenu (#5318).
        let siennes: Vec<i64> = pistes
            .iter()
            .filter(|(_, chemin)| sous_le_dossier_stocke(chemin, &d.dossier))
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
    let rendus: Vec<i64> = std::iter::once(cible)
        .chain(recrees.iter().copied())
        .collect();
    retirer_les_sous_titres(db, &rendus, &marqueur)?;
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
///
/// 🔴 La bibliothèque **LOCALE** seulement, et cela se lit sur la SOURCE des
/// deux côtés — la piste ET l'album. Le chemin de fichier ne tenait pas ce
/// rôle : une source distante peut en porter un.
pub fn lister(db: &Arc<dyn DbBackend>) -> Result<Vec<CoffretListe>, TuneError> {
    // Le chemin de [`chemin_ouvrable`] : un coffret réuni depuis des feuilles
    // CUE n'a aucune piste à `file_path` (#5317).
    let sql = format!(
        "SELECT t.album_id, COUNT(DISTINCT t.disc_number), MIN({c}), MAX({c}) \
         FROM tracks t JOIN albums a ON a.id = t.album_id \
         WHERE {piste_locale} AND {album_local} \
           AND {c} IS NOT NULL AND {caches} \
         GROUP BY t.album_id",
        piste_locale = crate::db::track_repo::sql::PISTE_LOCALE,
        album_local = crate::db::track_repo::sql::est_local("a"),
        caches = super::facet_filter::hidden_albums_excluded(),
        c = chemin_ouvrable!()
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
        let range_disque_par_disque =
            disques >= 2 && !meme_dossier(dossier_de(&premier), dossier_de(&dernier));
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
