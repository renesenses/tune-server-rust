//! #8160 — la trace DATÉE du spectrogramme conservé d'une piste (table
//! `spectrogrammes`, migrations 143 / PG 109).
//!
//! L'image reste un PNG du cache plafonné des spectrogrammes (#7857). Cette
//! table dit, pour la fiche technique d'une piste, QUELLE image en est la
//! dernière, QUAND elle a été calculée, combien elle pèse, et pour QUEL état du
//! fichier audio : sa signature (taille et date de modification). Une image
//! dont la signature ne correspond plus au fichier d'aujourd'hui est périmée et
//! se recalcule.
//!
//! Une ligne par piste. Écrire une nouvelle image pour une piste rend le nom de
//! l'ancienne, que l'appelant retire du cache.

use serde::Serialize;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::Engine;

/// La dernière image calculée pour une piste, telle que la table la garde.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spectrogramme {
    pub track_id: i64,
    /// Nom du PNG dans le dossier du cache des spectrogrammes.
    pub fichier: String,
    pub largeur: i64,
    pub hauteur: i64,
    /// Taille du PNG, en octets.
    pub octets: i64,
    /// La signature du fichier audio au moment du calcul ([`signature`]).
    pub signature: String,
    pub taille_fichier: Option<i64>,
    pub mtime_fichier: Option<f64>,
    pub duree_calcul_ms: Option<i64>,
    /// Date du calcul, UTC, `AAAA-MM-JJTHH:MM:SSZ`.
    pub calcule_le: String,
}

/// La signature d'un fichier audio : sa taille et sa date de modification à
/// la nanoseconde, `"<taille>:<mtime_ns>"`. Stable d'une version de Rust à
/// l'autre (pas de hachage), lisible à l'œil. Un fichier réécrit change de
/// taille ou de date : l'image calculée avant ne vaut plus pour lui.
pub fn signature(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}:{mtime}", meta.len())
}

/// La date de modification d'un fichier, en secondes (avec la fraction).
pub fn mtime_secondes(meta: &std::fs::Metadata) -> Option<f64> {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

fn marque(engine: Engine, rang: usize) -> String {
    match engine {
        Engine::Sqlite => "?".to_string(),
        Engine::Postgres => format!("${rang}"),
    }
}

/// L'heure courante au format des autres dates de la base.
pub fn maintenant() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

const COLONNES: &str = "track_id, fichier, largeur, hauteur, octets, signature, \
     taille_fichier, mtime_fichier, duree_calcul_ms, calcule_le";

fn ligne_en_spectrogramme(l: &[SqlValue]) -> Option<Spectrogramme> {
    Some(Spectrogramme {
        track_id: l.first()?.as_i64()?,
        fichier: l.get(1)?.as_string()?,
        largeur: l.get(2)?.as_i64()?,
        hauteur: l.get(3)?.as_i64()?,
        octets: l.get(4)?.as_i64()?,
        signature: l.get(5)?.as_string()?,
        taille_fichier: l.get(6).and_then(SqlValue::as_i64),
        mtime_fichier: l.get(7).and_then(SqlValue::as_f64),
        duree_calcul_ms: l.get(8).and_then(SqlValue::as_i64),
        calcule_le: l.get(9)?.as_string().unwrap_or_default(),
    })
}

/// La dernière image calculée pour la piste, ou `None`.
pub fn lire(db: &dyn DbBackend, track_id: i64) -> Result<Option<Spectrogramme>, String> {
    let sql = format!(
        "SELECT {COLONNES} FROM spectrogrammes WHERE track_id = {}",
        marque(db.engine(), 1)
    );
    let ligne = db.query_one(&sql, &[&track_id as &dyn ToSqlValue])?;
    Ok(ligne.as_deref().and_then(ligne_en_spectrogramme))
}

/// Enregistre la dernière image d'une piste (crée ou remplace la ligne).
/// Rend le nom de l'image PRÉCÉDENTE quand il diffère du nouveau : l'appelant
/// la retire du cache, elle ne sert plus.
pub fn enregistrer(db: &dyn DbBackend, s: &Spectrogramme) -> Result<Option<String>, String> {
    let avant = lire(db, s.track_id)?.map(|a| a.fichier);
    let e = db.engine();
    let marques: Vec<String> = (1..=10).map(|i| marque(e, i)).collect();
    let sql = format!(
        "INSERT INTO spectrogrammes ({COLONNES}) VALUES ({}) \
         ON CONFLICT (track_id) DO UPDATE SET \
         fichier = excluded.fichier, largeur = excluded.largeur, \
         hauteur = excluded.hauteur, octets = excluded.octets, \
         signature = excluded.signature, taille_fichier = excluded.taille_fichier, \
         mtime_fichier = excluded.mtime_fichier, \
         duree_calcul_ms = excluded.duree_calcul_ms, calcule_le = excluded.calcule_le",
        marques.join(", ")
    );
    db.execute(
        &sql,
        &[
            &s.track_id as &dyn ToSqlValue,
            &s.fichier,
            &s.largeur,
            &s.hauteur,
            &s.octets,
            &s.signature,
            &s.taille_fichier,
            &s.mtime_fichier,
            &s.duree_calcul_ms,
            &s.calcule_le,
        ],
    )?;
    Ok(avant.filter(|a| *a != s.fichier))
}

/// Oublie la ligne d'une piste (son image a disparu du cache).
pub fn oublier(db: &dyn DbBackend, track_id: i64) -> Result<bool, String> {
    let sql = format!(
        "DELETE FROM spectrogrammes WHERE track_id = {}",
        marque(db.engine(), 1)
    );
    Ok(db.execute(&sql, &[&track_id as &dyn ToSqlValue])? > 0)
}

/// Oublie les lignes des images évincées du cache. Rend le nombre de lignes
/// retirées.
pub fn oublier_les_fichiers(db: &dyn DbBackend, fichiers: &[String]) -> Result<usize, String> {
    let mut n = 0;
    let sql = format!(
        "DELETE FROM spectrogrammes WHERE fichier = {}",
        marque(db.engine(), 1)
    );
    for f in fichiers {
        n += db.execute(&sql, &[f as &dyn ToSqlValue])?;
    }
    Ok(n)
}

#[cfg(test)]
#[path = "spectrogrammes_tests_8160.rs"]
mod tests;
