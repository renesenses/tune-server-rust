//! #8026 — les crédits lus dans les balises, pour la musique DÉJÀ scannée.
//!
//! Depuis #7766, le scan écrit les lignes `track_credits` `source = 'balises'`
//! de chaque fichier neuf ou retouché. Une bibliothèque scannée avant n'en a
//! aucune tant que ses fichiers ne changent pas : la page artiste, qui lit
//! `track_credits`, n'y voit ni compositeur, ni chef, ni producteur.
//!
//! Deux passes, au démarrage, hors du chemin du démarrage :
//!
//! 1. **Le rattrapage** ([`rattraper_les_credits_des_balises`]) dérive les
//!    lignes `balises` de la `track_metadata` DÉJÀ en base, par
//!    [`credits_balises::ecrire_lot`] : aucun fichier n'est rouvert, le scan
//!    ne change pas de durée. Seules les pistes qui portent une clé de crédit
//!    et aucune ligne `balises` sont prises. Le marqueur
//!    [`CLE_RATTRAPAGE_CREDITS_BALISES_8026`] (`settings`) est posé quand la
//!    passe va au bout sans erreur : elle ne se rejoue pas.
//! 2. **Le rattachement** ([`rattacher_les_fiches`]) donne leur `artist_id`
//!    aux lignes `balises` qui n'en ont pas, par le nom de la fiche : d'abord
//!    à l'identique, sinon par [`cle_artiste`] (sans casse ni accents),
//!    quand une seule fiche porte cette clé. La page artiste
//!    (`artist_credits`, `credits_hors_discographie`) retrouve alors
//!    « Gunter Hermanns » balisé sans tréma sur la fiche « Günter Hermanns ».
//!    Le scan n'écrit pas `artist_id` (deux requêtes par crédit) : la passe
//!    se refait à chaque démarrage, sur les seules lignes encore sans fiche.

use std::collections::HashMap;
use std::sync::Arc;

use super::credits_balises::{self, CLES_DE_CREDIT};
use super::source_des_credits::BALISES;
use crate::db::artist_repo::cle_artiste;
use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::engine::Engine;
use crate::db::settings_repo::SettingsRepo;

/// Clé de `settings` qui marque le rattrapage comme fait.
pub const CLE_RATTRAPAGE_CREDITS_BALISES_8026: &str = "rattrapage_credits_balises_8026";

/// Pistes écrites par lot : la passe cède la main entre deux lots.
const PISTES_PAR_LOT: usize = 400;

/// `track_id` est un entier sous SQLite, du BIGINT ou du TEXT sous
/// PostgreSQL selon l'histoire de la base (migration 012) : comparé en texte.
fn entier(v: &SqlValue) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_string()?.trim().parse::<i64>().ok())
}

fn liste_des_cles() -> String {
    CLES_DE_CREDIT
        .iter()
        .map(|(cle, _)| format!("'{cle}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Les clés de crédit des pistes qui n'ont aucune ligne `balises` dans
/// `track_credits`. UNE requête pour toute la bibliothèque : un filtre
/// `track_id IN (…)` par lot ne servirait d'aucun index sous PostgreSQL
/// (comparaison en texte) et relirait la table à chaque lot.
pub fn sql_credits_des_pistes_sans_lignes_balises(engine: Engine) -> String {
    let jointure = match engine {
        Engine::Postgres => "CAST(tb.track_id AS TEXT) = CAST(tm.track_id AS TEXT)",
        Engine::Sqlite => "tb.track_id = tm.track_id",
    };
    format!(
        "SELECT tm.track_id, tm.key, tm.value FROM track_metadata tm \
         WHERE tm.key IN ({cles}) \
         AND NOT EXISTS (SELECT 1 FROM track_credits tb WHERE {jointure} AND tb.source = '{BALISES}')",
        cles = liste_des_cles()
    )
}

/// Bilan d'un rattrapage.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bilan {
    /// La passe était déjà faite (marqueur posé) : rien n'a été lu.
    pub deja_fait: bool,
    /// Pistes passées par l'écriture.
    pub pistes: usize,
    /// Lignes `balises` écrites.
    pub lignes: usize,
}

/// Écrit les lignes `balises` de toutes les pistes déjà scannées qui n'en ont
/// pas, depuis leur `track_metadata`. Une fois : le marqueur est posé au
/// bout. Une piste en échec est journalisée par `ecrire_lot` et n'arrête pas
/// la passe ; une lecture en échec l'arrête SANS poser le marqueur, pour
/// qu'elle reprenne au démarrage suivant (les pistes déjà faites ne sont
/// plus candidates).
pub fn rattraper_les_credits_des_balises(db: &Arc<dyn DbBackend>) -> Result<Bilan, String> {
    let reglages = SettingsRepo::with_backend(db.clone());
    if reglages.get(CLE_RATTRAPAGE_CREDITS_BALISES_8026)?.is_some() {
        return Ok(Bilan {
            deja_fait: true,
            ..Bilan::default()
        });
    }
    let mut par_piste: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for ligne in db.query_many(
        &sql_credits_des_pistes_sans_lignes_balises(db.engine()),
        &[],
    )? {
        let (Some(id), Some(cle), Some(valeur)) = (
            ligne.first().and_then(entier),
            ligne.get(1).and_then(SqlValue::as_string),
            ligne.get(2).and_then(SqlValue::as_string),
        ) else {
            continue;
        };
        par_piste.entry(id).or_default().insert(cle, valeur);
    }
    let mut pistes: Vec<(i64, HashMap<String, String>)> = par_piste.into_iter().collect();
    pistes.sort_unstable_by_key(|(id, _)| *id);
    let mut bilan = Bilan::default();
    for lot in pistes.chunks(PISTES_PAR_LOT) {
        bilan.pistes += lot.len();
        bilan.lignes += credits_balises::ecrire_lot(db, lot);
    }
    reglages.set(CLE_RATTRAPAGE_CREDITS_BALISES_8026, "fait")?;
    Ok(bilan)
}

/// Donne leur fiche aux lignes `balises` qui n'en ont pas. Le nom identique
/// d'abord, s'il est celui d'UNE seule fiche (un nom identique à deux fiches
/// ne se rattache pas, même par la clé) ; sinon la [`cle_artiste`] du nom,
/// si UNE seule fiche la porte (« Bjork » et « Björk » : on ne choisit pas).
/// Rend le nombre de lignes rattachées.
pub fn rattacher_les_fiches(db: &Arc<dyn DbBackend>) -> Result<usize, String> {
    // Les NOMS distincts, pas les lignes : un compositeur sans fiche cité sur
    // mille pistes coûte une ligne lue et aucune écriture.
    let noms: Vec<String> = db
        .query_many(
            "SELECT DISTINCT artist_name FROM track_credits WHERE source = ? AND artist_id IS NULL",
            &[&BALISES as &dyn ToSqlValue],
        )?
        .iter()
        .filter_map(|l| l.first().and_then(SqlValue::as_string))
        .collect();
    if noms.is_empty() {
        return Ok(0);
    }
    // `None` : plusieurs fiches portent ce nom, ou cette clé. On ne choisit
    // pas : deux « John Smith » restent sans fiche.
    let mut par_nom: HashMap<String, Option<i64>> = HashMap::new();
    let mut par_cle: HashMap<String, Option<i64>> = HashMap::new();
    let unique = |carte: &mut HashMap<String, Option<i64>>, cle: String, id: i64| {
        carte
            .entry(cle)
            .and_modify(|deja| {
                if *deja != Some(id) {
                    *deja = None;
                }
            })
            .or_insert(Some(id));
    };
    for fiche in db.query_many("SELECT id, name FROM artists", &[])? {
        let (Some(id), Some(nom)) = (
            fiche.first().and_then(entier),
            fiche.get(1).and_then(SqlValue::as_string),
        ) else {
            continue;
        };
        unique(&mut par_cle, cle_artiste(&nom), id);
        unique(&mut par_nom, nom, id);
    }
    let mut rattachees = 0usize;
    for nom in noms {
        let fiche = match par_nom.get(&nom) {
            Some(exact) => *exact,
            None => par_cle.get(&cle_artiste(&nom)).copied().flatten(),
        };
        let Some(artist_id) = fiche else {
            continue;
        };
        match db.execute(
            "UPDATE track_credits SET artist_id = ? \
             WHERE source = ? AND artist_id IS NULL AND artist_name = ?",
            &[
                &artist_id as &dyn ToSqlValue,
                &BALISES as &dyn ToSqlValue,
                &nom as &dyn ToSqlValue,
            ],
        ) {
            Ok(n) => rattachees += n,
            Err(e) => tracing::warn!(nom = %nom, error = %e, "credits_balises_rattachement_echec"),
        }
    }
    Ok(rattachees)
}

/// Les deux passes, journalisées. Une erreur se journalise, rien ne remonte :
/// appelée au démarrage, dans une tâche à part.
pub fn passes_journalisees(db: &Arc<dyn DbBackend>) {
    match rattraper_les_credits_des_balises(db) {
        Ok(b) if b.deja_fait => {}
        Ok(b) => tracing::info!(
            pistes = b.pistes,
            lignes = b.lignes,
            "credits_balises_rattrapage_8026_fait"
        ),
        Err(e) => tracing::warn!(error = %e, "credits_balises_rattrapage_8026_echec"),
    }
    match rattacher_les_fiches(db) {
        Ok(0) => {}
        Ok(n) => tracing::info!(lignes = n, "credits_balises_fiches_rattachees_8026"),
        Err(e) => tracing::warn!(error = %e, "credits_balises_rattachement_8026_echec"),
    }
}

#[cfg(test)]
#[path = "rattrapage_credits_balises_8026_tests.rs"]
mod tests;
