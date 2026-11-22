//! #9265 (fil 2969) — les artistes d'un crédit composé.
//!
//! Une balise « A & B » ou « A feat. C » donne UNE ligne `artists` à elle
//! seule : la grille Artistes montrait « A », « A & B » et « A feat. C »
//! côte à côte. Cette table relie la fiche du crédit composé (`credit_id`)
//! aux fiches des artistes qui le composent (`artiste_id`, dans l'ordre du
//! crédit, `rang`). La grille masque alors le crédit relié et montre ses
//! artistes ; la page d'un artiste liste aussi les albums et pistes de ses
//! crédits reliés. Rien n'est réécrit : `albums.artist_id` et
//! `tracks.artist_id` gardent le crédit d'origine, qui reste affiché sur
//! l'album et la piste.
//!
//! # Règle STRICTE (décision de Bertrand du 20/11)
//!
//! Le découpage est celui de [`analyze_artist_credit`] (séparateurs forts et
//! risqués, liste blanche `KNOWN_GROUP_NAMES` + réglage
//! `artist_split_allowlist`). Mais un crédit n'est relié que si CHACUN de ses
//! morceaux a déjà sa propre fiche (rapprochement par [`cle_artiste`]), et
//! qu'il en reste au moins deux distincts. Aucune fiche n'est créée, y
//! compris pour « feat. ».
//!
//! Mesure du 20/11 sur le .18 (aperçu `/system/artist-split-preview`) : 186
//! crédits sur 2 968 seraient découpés, mais le découpage risqué y produit
//! des morceaux qui ne sont pas des artistes (« Davis, Miles », « Jordi
//! Savall - violes de gambe », « … Orchestra & Chorus », « …, piano »). Le
//! MBID n'est pas un critère : des collaborations évidentes en portent un.
//! L'exigence « toutes les fiches existent » écarte ces faux positifs sans
//! liste à tenir : « Davis » et « Miles » ne sont pas des artistes de la
//! bibliothèque.
//!
//! # Table dérivée, sans numéro de migration
//!
//! Recalculée en entier depuis les seuls noms d'`artists` ([`recalculer`]) :
//! en fin de scan et au démarrage (hors du chemin du démarrage). Idempotente :
//! sans changement, rien n'est écrit. Sans numéro (les numéros SQLite 157 et
//! suivants sont disputés dans le lot rc13), sur le modèle de
//! `alertes_douteuses_ignorees` : `init_schema` et passe finale de
//! `run_migrations` (SQLite), `ENSURE_TABLES` et `PG_FULL_SCHEMA`
//! (PostgreSQL). Non copiée à la bascule : recalculée au démarrage suivant.
//! La supprimer ne perd rien.

use super::artist_repo::cle_artiste;
use super::backend::{DbBackend, ToSqlValue};
use crate::metadata::artist_split::analyze_artist_credit;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/artistes_des_credits.sql");

/// La page d'un artiste cherche ses crédits par `artiste_id`.
pub const SQL_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_artistes_des_credits_artiste \
     ON artistes_des_credits(artiste_id)";

/// Une ligne de la table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Liaison {
    pub credit_id: i64,
    pub artiste_id: i64,
    pub rang: i64,
}

/// Ce qu'une passe a trouvé et fait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bilan {
    /// Crédits composés reliés à leurs artistes.
    pub credits: usize,
    /// Lignes de la table.
    pub liaisons: usize,
    /// Faux quand la table était déjà à jour (rien d'écrit).
    pub ecrit: bool,
}

/// Les liaisons que mérite cette liste d'artistes `(id, nom)`. Pure : ni
/// base, ni réseau. Voir la règle stricte en tête de module.
pub fn planifier(artistes: &[(i64, String)], liste_blanche: &[String]) -> Vec<Liaison> {
    // Clé de rapprochement → fiche. Deux fiches de même clé (doublon pas
    // encore absorbé) : la plus ancienne, pour un résultat stable.
    let mut par_cle: HashMap<String, i64> = HashMap::with_capacity(artistes.len());
    for (id, nom) in artistes {
        let cle = cle_artiste(nom);
        if cle.is_empty() {
            continue;
        }
        par_cle
            .entry(cle)
            .and_modify(|v| *v = (*v).min(*id))
            .or_insert(*id);
    }
    let mut liaisons = Vec::new();
    for (credit_id, nom) in artistes {
        let analyse = analyze_artist_credit(nom, liste_blanche, true);
        if !analyse.would_split() {
            continue;
        }
        let mut membres: Vec<i64> = Vec::with_capacity(analyse.tokens.len());
        let mut tous = true;
        for morceau in &analyse.tokens {
            match par_cle.get(&cle_artiste(morceau)) {
                Some(id) if id != credit_id => {
                    if !membres.contains(id) {
                        membres.push(*id);
                    }
                }
                _ => {
                    tous = false;
                    break;
                }
            }
        }
        if !tous || membres.len() < 2 {
            continue;
        }
        for (rang, artiste_id) in membres.into_iter().enumerate() {
            liaisons.push(Liaison {
                credit_id: *credit_id,
                artiste_id,
                rang: rang as i64,
            });
        }
    }
    liaisons
}

/// La liste blanche du réglage `artist_split_allowlist` (tableau JSON).
pub fn liste_blanche_du_reglage(db: &Arc<dyn DbBackend>) -> Vec<String> {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("artist_split_allowlist")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// Les liaisons en base.
pub fn lister(db: &Arc<dyn DbBackend>) -> Result<Vec<Liaison>, String> {
    Ok(db
        .query_many(
            "SELECT credit_id, artiste_id, rang FROM artistes_des_credits",
            &[],
        )?
        .iter()
        .filter_map(|l| {
            Some(Liaison {
                credit_id: l.first()?.as_i64()?,
                artiste_id: l.get(1)?.as_i64()?,
                rang: l.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

/// Recalcule toute la table depuis `artists`. Idempotente : une table déjà
/// à jour n'est pas réécrite. Tout ou rien (une transaction).
pub fn recalculer(db: &Arc<dyn DbBackend>, liste_blanche: &[String]) -> Result<Bilan, String> {
    let artistes: Vec<(i64, String)> = db
        .query_many("SELECT id, name FROM artists", &[])?
        .iter()
        .filter_map(|l| Some((l.first()?.as_i64()?, l.get(1)?.as_string()?)))
        .collect();
    let mut voulues = planifier(&artistes, liste_blanche);
    voulues.sort();
    let credits = voulues
        .iter()
        .map(|l| l.credit_id)
        .collect::<HashSet<_>>()
        .len();
    let mut presentes = lister(db)?;
    presentes.sort();
    let mut bilan = Bilan {
        credits,
        liaisons: voulues.len(),
        ecrit: false,
    };
    if presentes == voulues {
        return Ok(bilan);
    }
    let inserer = match db.engine() {
        super::engine::Engine::Postgres => {
            "INSERT INTO artistes_des_credits (credit_id, artiste_id, rang) VALUES ($1, $2, $3)"
        }
        super::engine::Engine::Sqlite => {
            "INSERT INTO artistes_des_credits (credit_id, artiste_id, rang) VALUES (?, ?, ?)"
        }
    };
    db.write_tx(&mut |tx| {
        tx.execute("DELETE FROM artistes_des_credits", &[])?;
        for l in &voulues {
            tx.execute(
                inserer,
                &[
                    &l.credit_id as &dyn ToSqlValue,
                    &l.artiste_id as &dyn ToSqlValue,
                    &l.rang as &dyn ToSqlValue,
                ],
            )?;
        }
        Ok(())
    })?;
    bilan.ecrit = true;
    Ok(bilan)
}

/// [`recalculer`] avec la liste blanche du réglage, journalisée. `moment` :
/// `demarrage` ou `scan`.
pub fn passe_journalisee(db: &Arc<dyn DbBackend>, moment: &str) {
    let liste_blanche = liste_blanche_du_reglage(db);
    match recalculer(db, &liste_blanche) {
        Ok(bilan) if bilan.ecrit => {
            tracing::info!(moment, ?bilan, "artistes_des_credits_recalcules")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(moment, error = %e, "artistes_des_credits_recalcul_echoue"),
    }
}

#[cfg(test)]
#[path = "artistes_des_credits_tests_9265.rs"]
mod tests_9265;
