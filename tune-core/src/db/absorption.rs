//! Les gestes communs d'une absorption — un enregistrement en absorbe un
//! autre qui désigne la même chose — partagés par les albums (BIB-A2) et les
//! artistes (BIB-C1). Portables SQLite / PostgreSQL, tolérants aux tables et
//! colonnes qu'une base ancienne n'a pas.

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Une base ancienne ou partielle : la table ou la colonne n'existe pas.
pub(crate) fn table_absente(e: &str) -> bool {
    e.contains("no such table") || e.contains("no such column") || e.contains("does not exist")
}

fn marque(db: &dyn DbBackend, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// `UPDATE {table} SET {colonne} = cible WHERE {colonne} = doublon [AND filtre]`.
pub(crate) fn repointer(
    db: &dyn DbBackend,
    table: &str,
    colonne: &str,
    filtre: Option<&str>,
    cible: i64,
    doublon: i64,
) -> Result<usize, String> {
    let filtre = filtre.map(|f| format!(" AND {f}")).unwrap_or_default();
    let sql = format!(
        "UPDATE {table} SET {colonne} = {} WHERE {colonne} = {}{filtre}",
        marque(db, 1),
        marque(db, 2)
    );
    let params: [&dyn ToSqlValue; 2] = [&cible, &doublon];
    match db.execute(&sql, &params) {
        Ok(n) => Ok(n),
        Err(e) if table_absente(&e) => {
            tracing::debug!(table, error = %e, "absorption_table_absente");
            Ok(0)
        }
        Err(e) => Err(e),
    }
}

/// Même chose pour une table à clé unique `(colonne, discriminant)` : la
/// ligne du doublon dont la cible possède déjà l'équivalent est retirée
/// d'abord — la cible garde la sienne — puis le reste est repointé.
/// `UPDATE OR IGNORE` n'existe pas sous PostgreSQL.
pub(crate) fn repointer_a_cle_unique(
    db: &dyn DbBackend,
    table: &str,
    colonne: &str,
    discriminant: &str,
    filtre: Option<&str>,
    cible: i64,
    doublon: i64,
) -> Result<usize, String> {
    let f = filtre.map(|f| format!(" AND {f}")).unwrap_or_default();
    let (p1, p2) = (marque(db, 1), marque(db, 2));
    let purge = format!(
        "DELETE FROM {table} WHERE {colonne} = {p1}{f} AND {discriminant} IN \
         (SELECT {discriminant} FROM {table} WHERE {colonne} = {p2}{f})"
    );
    let params: [&dyn ToSqlValue; 2] = [&doublon, &cible];
    match db.execute(&purge, &params) {
        Ok(_) => {}
        Err(e) if table_absente(&e) => return Ok(0),
        Err(e) => return Err(e),
    }
    repointer(db, table, colonne, filtre, cible, doublon)
}

/// Les champs de la cible restés vides prennent la valeur du doublon —
/// jamais l'inverse : un champ renseigné sur la cible ne cède pas.
pub(crate) fn reprendre_les_champs_vides(
    db: &dyn DbBackend,
    table: &str,
    champs: &[&str],
    cible: i64,
    doublon: i64,
) -> Result<usize, String> {
    let (p1, p2, p3) = (marque(db, 1), marque(db, 2), marque(db, 3));
    let params: [&dyn ToSqlValue; 3] = [&doublon, &cible, &doublon];
    let mut repris = 0usize;
    for champ in champs {
        let sql = format!(
            "UPDATE {table} SET {champ} = (SELECT d.{champ} FROM {table} d WHERE d.id = {p1}) \
             WHERE id = {p2} AND ({champ} IS NULL OR CAST({champ} AS TEXT) = '') \
               AND EXISTS (SELECT 1 FROM {table} d WHERE d.id = {p3} \
                           AND d.{champ} IS NOT NULL AND CAST(d.{champ} AS TEXT) <> '')"
        );
        match db.execute(&sql, &params) {
            Ok(n) => repris += n,
            Err(e) if table_absente(&e) => {
                tracing::debug!(table, champ, error = %e, "absorption_champ_absent");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(repris)
}

/// Une valeur texte du doublon recalée sur celle de la cible :
/// `UPDATE {table} SET {colonne} = nom_cible WHERE {ancre} = cible AND {colonne} = nom_doublon`.
pub(crate) fn recaler_le_texte(
    db: &dyn DbBackend,
    table: &str,
    colonne: &str,
    ancre: &str,
    cible: i64,
    nom_cible: &str,
    nom_doublon: &str,
) -> Result<usize, String> {
    let sql = format!(
        "UPDATE {table} SET {colonne} = {} WHERE {ancre} = {} AND {colonne} = {}",
        marque(db, 1),
        marque(db, 2),
        marque(db, 3)
    );
    let params: [&dyn ToSqlValue; 3] = [&nom_cible, &cible, &nom_doublon];
    match db.execute(&sql, &params) {
        Ok(n) => Ok(n),
        Err(e) if table_absente(&e) => Ok(0),
        Err(e) => Err(e),
    }
}
