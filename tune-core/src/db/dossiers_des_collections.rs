//! Les dossiers « Collections » suivent leurs albums (#5527, #5528).
//!
//! Un dossier est une liste d'identifiants d'albums rangée dans le réglage
//! `collections` (JSON), avec, sous `album_labels`, le titre et l'artiste de
//! chaque album au moment où il a été rangé (#901). Un identifiant qui ne
//! désigne plus aucun album y reste, et l'écran le liste parmi les
//! « manquants ».
//!
//! Lulu (JLuc, fil 1891, v0.9.168) en a relevé deux conséquences :
//!
//! - **#5527** — il range l'album vivant, et l'ancien identifiant du même
//!   album reste listé comme manquant, sous le même titre ;
//! - **#5528** — des disques réunis en un seul album (des opéras) restent
//!   listés comme manquants, sans rien qui dise où ils sont passés.
//!
//! Règles arbitrées par Bertrand le 30/09/2026 :
//!
//! 1. Quand une mise à jour de scan fait passer une piste d'un album rangé
//!    vers un autre (même fichier, `album_id` changé), l'étiquette de l'album
//!    de départ note l'album qui a reçu la piste (`merged_into`).
//! 2. Quand les albums vidés sont supprimés, les dossiers SUIVENT : un album
//!    disparu dont les pistes sont toutes allées dans UN seul album vivant y
//!    est remplacé par lui ([`suivre_les_albums_disparus`]).
//! 3. Ranger un album retire du dossier les identifiants MORTS du même album
//!    — même artiste, même titre, à la casse, aux accents, à la ponctuation et
//!    au numéro de disque près ([`cle_d_album`]).
//!
//! ⚠️ On ne purge jamais sur une simple lecture, et on ne remplace jamais sur
//! une ressemblance de titre : un album peut manquer parce qu'un disque n'est
//! pas monté. Le suivi n'agit que sur un déplacement de pistes CONSTATÉ, et le
//! rapprochement par titre seulement quand l'utilisateur range l'album vivant
//! lui-même.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::album_repo::AlbumRepo;
use super::backend::{DbBackend, ToSqlValue};
use super::engine::fold_diacritics;
use super::settings_repo::SettingsRepo;
use crate::TuneError;

/// Le réglage qui porte les dossiers.
pub const REGLAGE: &str = "collections";

/// Champ STOCKÉ de chaque dossier : le nom des albums au moment du rangement,
/// indexé par identifiant (#901).
pub const ETIQUETTES: &str = "album_labels";

/// Dans une étiquette : les albums qui ont reçu des pistes de cet album
/// (#5528). Une liste, parce qu'un album peut être réparti entre plusieurs :
/// le suivi n'agit alors pas.
pub const REUNI_DANS: &str = "merged_into";

// ---------------------------------------------------------------------------
// La clé d'un album : ce qui fait dire « c'est le même »
// ---------------------------------------------------------------------------

/// Un texte sans casse, sans accents et sans ponctuation : les mots seuls,
/// séparés d'une espace.
fn cle_de_texte(s: &str) -> String {
    let plie = fold_diacritics(s).to_lowercase();
    plie.split(|c: char| !c.is_alphanumeric())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Le titre sans son numéro de disque, en fin (« Tosca, CD1 », « Tosca
/// (Disc 2) ») ou en tête (« CD1 - Tosca »). Mêmes règles que la détection
/// des coffrets — [`crate::metadata::coffrets::marqueur_final`] — pour que
/// les deux ne divergent pas : `vol` n'est pas un disque, et un numéro est
/// exigé.
fn titre_sans_disque(titre: &str) -> String {
    use crate::metadata::coffrets::{marqueur_de_tete, marqueur_final};
    marqueur_final(titre)
        .or_else(|| marqueur_de_tete(titre))
        .map(|(socle, _)| socle)
        .unwrap_or_else(|| titre.to_string())
}

/// La clé d'identité d'un album pour les dossiers, ou `None` quand il n'y a
/// pas de quoi en former une.
///
/// Même artiste et même titre, en ignorant la casse, les accents, la
/// ponctuation et un suffixe de disque (« CD1 », « Disc 2 », « (Disc 1) »).
///
/// ⚠️ Pourquoi pas la clé de `AlbumDistinctRepo::reconcile` : celle-ci
/// compare en SQL `LOWER(titre)` et `LOWER(artiste)`
/// (`favorites_reconcile::find_album_by_identity`). Elle ne plie ni les
/// accents, ni la ponctuation, ni le numéro de disque — or c'est précisément
/// ce qui sépare « Tosca, CD1 » de « Tosca ».
///
/// Un artiste inconnu ne forme pas de clé : deux albums sans artiste du même
/// titre ne sont pas, pour autant, le même album.
pub fn cle_d_album(titre: &str, artiste: Option<&str>) -> Option<String> {
    let titre = cle_de_texte(&titre_sans_disque(titre));
    let artiste = cle_de_texte(artiste?);
    if titre.is_empty() || artiste.is_empty() {
        return None;
    }
    Some(format!("{artiste}\u{1f}{titre}"))
}

/// La clé d'une étiquette conservée (`{"title", "artist"}`).
pub fn cle_d_etiquette(etiquette: &Value) -> Option<String> {
    let titre = etiquette.get("title").and_then(|v| v.as_str())?;
    let artiste = etiquette.get("artist").and_then(|v| v.as_str());
    cle_d_album(titre, artiste)
}

// ---------------------------------------------------------------------------
// Lecture et écriture du réglage
// ---------------------------------------------------------------------------

fn lire(db: &Arc<dyn DbBackend>) -> Result<Option<Vec<Value>>, TuneError> {
    let brut = SettingsRepo::with_backend(db.clone())
        .get(REGLAGE)
        .map_err(TuneError::from)?;
    Ok(brut.and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok()))
}

fn ecrire(db: &Arc<dyn DbBackend>, dossiers: &[Value]) -> Result<(), TuneError> {
    let texte = serde_json::to_string(dossiers).map_err(|e| TuneError::from(e.to_string()))?;
    SettingsRepo::with_backend(db.clone())
        .set(REGLAGE, &texte)
        .map_err(TuneError::from)
}

fn ids_du_dossier(dossier: &Value) -> Vec<i64> {
    dossier
        .get("album_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

fn etiquettes_du_dossier(dossier: &Value) -> Map<String, Value> {
    dossier
        .get(ETIQUETTES)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Les albums notés comme ayant reçu des pistes de cette étiquette.
pub fn reunis_dans(etiquette: &Value) -> Vec<i64> {
    etiquette
        .get(REUNI_DANS)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

/// L'étiquette d'un album vivant, lue en base.
fn etiquette_vivante(repo: &AlbumRepo, id: i64) -> Option<Value> {
    match repo.get(id) {
        Ok(Some(a)) => Some(json!({ "title": a.title, "artist": a.artist_name })),
        _ => None,
    }
}

/// Tous les identifiants rangés dans au moins un dossier.
pub fn ids_ranges(dossiers: &[Value]) -> HashSet<i64> {
    dossiers.iter().flat_map(ids_du_dossier).collect()
}

// ---------------------------------------------------------------------------
// 1. Noter le déplacement
// ---------------------------------------------------------------------------

/// Les identifiants d'albums rangés dans au moins un dossier — `None` quand
/// aucun dossier n'en contient : l'appelant n'a alors rien à relever.
pub fn albums_ranges(db: &Arc<dyn DbBackend>) -> Result<Option<HashSet<i64>>, TuneError> {
    let Some(dossiers) = lire(db)? else {
        return Ok(None);
    };
    let ids = ids_ranges(&dossiers);
    Ok(if ids.is_empty() { None } else { Some(ids) })
}

/// Note, dans l'étiquette de chaque album rangé `ancien`, l'album `nouveau`
/// qui vient d'en recevoir des pistes. Rend le nombre d'étiquettes modifiées.
///
/// L'étiquette manquante est d'abord relue en base (l'album de départ vit
/// encore à cet instant) : sans elle, l'album disparu n'aurait plus de nom.
pub fn noter_les_deplacements(
    db: &Arc<dyn DbBackend>,
    deplacements: &[(i64, i64)],
) -> Result<usize, TuneError> {
    if deplacements.is_empty() {
        return Ok(0);
    }
    let Some(mut dossiers) = lire(db)? else {
        return Ok(0);
    };
    let repo = AlbumRepo::with_backend(db.clone());
    let mut changees = 0usize;
    for dossier in dossiers.iter_mut() {
        let ids: HashSet<i64> = ids_du_dossier(dossier).into_iter().collect();
        let mut etiquettes = etiquettes_du_dossier(dossier);
        let mut change = false;
        for &(ancien, nouveau) in deplacements {
            if ancien == nouveau || !ids.contains(&ancien) {
                continue;
            }
            let cle = ancien.to_string();
            let mut etiquette = match etiquettes.get(&cle) {
                Some(e) if e.is_object() => e.clone(),
                _ => etiquette_vivante(&repo, ancien).unwrap_or_else(|| json!({})),
            };
            let mut recus: BTreeSet<i64> = reunis_dans(&etiquette).into_iter().collect();
            if !recus.insert(nouveau) {
                continue;
            }
            if let Some(obj) = etiquette.as_object_mut() {
                obj.insert(
                    REUNI_DANS.into(),
                    json!(recus.into_iter().collect::<Vec<_>>()),
                );
            }
            etiquettes.insert(cle, etiquette);
            change = true;
        }
        if change {
            if let Some(obj) = dossier.as_object_mut() {
                obj.insert(ETIQUETTES.into(), Value::Object(etiquettes));
            }
            changees += 1;
        }
    }
    if changees > 0 {
        ecrire(db, &dossiers)?;
    }
    Ok(changees)
}

/// Les déplacements `(album de départ, album d'arrivée)` que la mise à jour de
/// ces pistes va produire, pour les seuls albums de départ rangés dans un
/// dossier. `pistes` : `(id de piste, album_id qu'on va écrire)`.
///
/// Lecture FORTE : le scan appelle ceci dans la transaction de son lot, et le
/// pool de lecture de SQLite ne verrait pas ce qu'elle a déjà écrit.
pub fn deplacements_a_venir(
    db: &Arc<dyn DbBackend>,
    pistes: &[(i64, i64)],
    ranges: &HashSet<i64>,
) -> Result<Vec<(i64, i64)>, TuneError> {
    let mut sortie: BTreeSet<(i64, i64)> = BTreeSet::new();
    let vers: HashMap<i64, i64> = pistes.iter().copied().collect();
    let ids: Vec<i64> = vers.keys().copied().collect();
    for paquet in ids.chunks(500) {
        let places: Vec<String> = (1..=paquet.len())
            .map(|i| match db.engine() {
                super::engine::Engine::Sqlite => "?".to_string(),
                super::engine::Engine::Postgres => format!("${i}"),
            })
            .collect();
        let sql = format!(
            "SELECT id, album_id FROM tracks WHERE id IN ({})",
            places.join(", ")
        );
        let params: Vec<&dyn ToSqlValue> = paquet.iter().map(|v| v as &dyn ToSqlValue).collect();
        for ligne in db
            .query_many_strong(&sql, &params)
            .map_err(TuneError::from)?
        {
            let (Some(piste), Some(ancien)) = (
                ligne.first().and_then(|v| v.as_i64()),
                ligne.get(1).and_then(|v| v.as_i64()),
            ) else {
                continue;
            };
            let Some(&nouveau) = vers.get(&piste) else {
                continue;
            };
            if ancien != nouveau && ranges.contains(&ancien) {
                sortie.insert((ancien, nouveau));
            }
        }
    }
    Ok(sortie.into_iter().collect())
}

// ---------------------------------------------------------------------------
// 2. Remplacer, et suivre les albums disparus
// ---------------------------------------------------------------------------

/// Remplace, dans UN dossier, `ancien` par `nouveau` : l'identifiant, sans
/// doublon, et l'étiquette — celle de `nouveau` relue en base, celle
/// d'`ancien` retirée (il n'est plus rangé, il ne « manque » plus).
fn remplacer_dans(
    dossier: &mut Value,
    ancien: i64,
    nouveau: i64,
    etiquette_nouveau: Option<&Value>,
) -> bool {
    let ids = ids_du_dossier(dossier);
    if !ids.contains(&ancien) {
        return false;
    }
    let mut sortie: Vec<i64> = Vec::with_capacity(ids.len());
    for id in ids {
        let id = if id == ancien { nouveau } else { id };
        if !sortie.contains(&id) {
            sortie.push(id);
        }
    }
    let mut etiquettes = etiquettes_du_dossier(dossier);
    etiquettes.remove(&ancien.to_string());
    if let Some(e) = etiquette_nouveau {
        etiquettes.insert(nouveau.to_string(), e.clone());
    }
    if let Some(obj) = dossier.as_object_mut() {
        obj.insert("album_ids".into(), json!(sortie));
        obj.insert(ETIQUETTES.into(), Value::Object(etiquettes));
    }
    true
}

/// Remplace `ancien` par `nouveau` dans tous les dossiers, étiquettes
/// comprises. Rend le nombre de dossiers réécrits.
///
/// C'est le chemin de [`AlbumRepo::absorber`] (fusion de doublons, coffrets) :
/// il réécrivait `album_ids` et laissait `album_labels` en l'état — l'album
/// cible n'y avait pas de nom, et disparaissait un jour sans en avoir (#5528).
pub fn remplacer_dans_les_dossiers(
    db: &Arc<dyn DbBackend>,
    ancien: i64,
    nouveau: i64,
) -> Result<usize, TuneError> {
    let Some(mut dossiers) = lire(db)? else {
        return Ok(0);
    };
    let etiquette = etiquette_vivante(&AlbumRepo::with_backend(db.clone()), nouveau);
    let mut reecrits = 0usize;
    for dossier in dossiers.iter_mut() {
        if remplacer_dans(dossier, ancien, nouveau, etiquette.as_ref()) {
            reecrits += 1;
        }
    }
    if reecrits > 0 {
        ecrire(db, &dossiers)?;
    }
    Ok(reecrits)
}

/// Ce que [`suivre_les_albums_disparus`] a fait.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BilanDuSuivi {
    /// `(album disparu, album qui le remplace)`, une fois par paire.
    pub remplaces: Vec<(i64, i64)>,
    /// Étiquettes d'albums TOUJOURS vivants dont la note a été effacée.
    pub notes_effacees: usize,
}

/// Les dossiers suivent les albums disparus (#5528).
///
/// Pour chaque album rangé dont l'étiquette note des albums d'arrivée :
///
/// - il n'existe plus, et UN SEUL des albums d'arrivée est vivant : il est
///   remplacé par celui-ci ;
/// - il existe encore : la note est effacée. Des pistes en sont parties, mais
///   il n'a pas disparu ; si le reste part plus tard, un nouveau déplacement
///   sera noté ;
/// - il n'existe plus et ses pistes sont parties dans plusieurs albums
///   vivants : rien ne se fait. Ce n'est pas une réunion, c'est un partage,
///   et on ne choisit pas à la place de l'utilisateur.
///
/// À appeler APRÈS la suppression des albums vidés : c'est elle qui établit
/// qu'un album a disparu. Aucun `GET` ne l'appelle.
pub fn suivre_les_albums_disparus(db: &Arc<dyn DbBackend>) -> Result<BilanDuSuivi, TuneError> {
    let Some(mut dossiers) = lire(db)? else {
        return Ok(BilanDuSuivi::default());
    };
    // Les albums concernés : notés, et leurs albums d'arrivée.
    let mut concernes: HashSet<i64> = HashSet::new();
    for dossier in &dossiers {
        let ids: HashSet<i64> = ids_du_dossier(dossier).into_iter().collect();
        for (cle, e) in etiquettes_du_dossier(dossier) {
            let recus = reunis_dans(&e);
            if recus.is_empty() {
                continue;
            }
            if let Ok(id) = cle.parse::<i64>()
                && ids.contains(&id)
            {
                concernes.insert(id);
                concernes.extend(recus);
            }
        }
    }
    if concernes.is_empty() {
        return Ok(BilanDuSuivi::default());
    }
    let repo = AlbumRepo::with_backend(db.clone());
    let vivants = repo.ids_existants(&concernes.iter().copied().collect::<Vec<_>>())?;

    let mut bilan = BilanDuSuivi::default();
    let mut etiquettes_vivantes: HashMap<i64, Option<Value>> = HashMap::new();
    let mut change = false;
    for dossier in dossiers.iter_mut() {
        let ids = ids_du_dossier(dossier);
        let etiquettes = etiquettes_du_dossier(dossier);
        let mut a_remplacer: Vec<(i64, i64)> = Vec::new();
        let mut a_effacer: Vec<String> = Vec::new();
        for id in &ids {
            let Some(e) = etiquettes.get(&id.to_string()) else {
                continue;
            };
            let recus = reunis_dans(e);
            if recus.is_empty() {
                continue;
            }
            if vivants.contains(id) {
                a_effacer.push(id.to_string());
                continue;
            }
            let arrivees: Vec<i64> = recus
                .into_iter()
                .filter(|r| r != id && vivants.contains(r))
                .collect();
            if let [seul] = arrivees.as_slice() {
                a_remplacer.push((*id, *seul));
            }
        }
        if !a_effacer.is_empty() {
            let mut etiquettes = etiquettes;
            for cle in &a_effacer {
                if let Some(obj) = etiquettes.get_mut(cle).and_then(|e| e.as_object_mut()) {
                    obj.remove(REUNI_DANS);
                }
            }
            if let Some(obj) = dossier.as_object_mut() {
                obj.insert(ETIQUETTES.into(), Value::Object(etiquettes));
            }
            bilan.notes_effacees += a_effacer.len();
            change = true;
        }
        for (ancien, nouveau) in a_remplacer {
            let etiquette = etiquettes_vivantes
                .entry(nouveau)
                .or_insert_with(|| etiquette_vivante(&repo, nouveau))
                .clone();
            if remplacer_dans(dossier, ancien, nouveau, etiquette.as_ref()) {
                if !bilan.remplaces.contains(&(ancien, nouveau)) {
                    bilan.remplaces.push((ancien, nouveau));
                }
                change = true;
            }
        }
    }
    if change {
        ecrire(db, &dossiers)?;
    }
    if !bilan.remplaces.is_empty() {
        tracing::info!(
            remplaces = ?bilan.remplaces,
            "collections_albums_disparus_suivis (#5528)"
        );
    }
    Ok(bilan)
}

/// [`suivre_les_albums_disparus`], dont un échec se journalise sans remonter :
/// il ne doit jamais faire échouer la purge qui l'appelle.
pub fn suivre_sans_echouer(db: &Arc<dyn DbBackend>) {
    if let Err(e) = suivre_les_albums_disparus(db) {
        tracing::warn!(error = %e, "collections_suivi_des_albums_disparus_echec (#5528)");
    }
}

#[cfg(test)]
#[path = "dossiers_des_collections_tests.rs"]
mod tests;
