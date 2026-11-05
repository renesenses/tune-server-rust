//! #7849 — les répartitions de la bibliothèque locale, servies par
//! `GET /library/stats` sous la clé `distributions` : par codec, par
//! fréquence d'échantillonnage, par décennie de sortie et par rôle de crédit.
//!
//! Chaque répartition est une liste de cases `{ value, tracks }` (plus
//! `people` pour les rôles). `value` vaut `null` pour « inconnu » : le client
//! traduit lui-même ce libellé.
//!
//! - codec : `tracks.format`, en minuscules ; vide ou absent → inconnu ;
//! - fréquence : `tracks.sample_rate` en Hz ; absente ou 0 → inconnue ;
//! - décennie : l'année de la piste, à défaut celle de son album ; absente ou
//!   nulle → inconnue ;
//! - rôle : `track_credits.role` en minuscules ; `tracks` compte les pistes
//!   distinctes, `people` les personnes distinctes (fiche d'artiste si elle
//!   existe, nom sinon).

use serde::Serialize;

use super::backend::{DbBackend, SqlValue};
use super::engine::Engine;

/// Une case d'une répartition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Case<V> {
    pub value: Option<V>,
    pub tracks: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub people: Option<i64>,
}

/// Les quatre répartitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repartitions {
    pub by_codec: Vec<Case<String>>,
    pub by_sample_rate: Vec<Case<i64>>,
    pub by_decade: Vec<Case<i64>>,
    pub by_credit_role: Vec<Case<String>>,
}

/// Un entier en PostgreSQL même si la colonne d'une ancienne bascule est
/// restée en TEXT ; tel quel en SQLite.
fn entier(engine: Engine, colonne: &str) -> String {
    match engine {
        Engine::Sqlite => colonne.to_string(),
        Engine::Postgres => format!("CAST({colonne} AS BIGINT)"),
    }
}

fn entier_de(v: Option<&SqlValue>) -> i64 {
    v.and_then(|v| v.as_i64()).unwrap_or(0)
}

fn lignes(db: &dyn DbBackend, sql: &str) -> Result<Vec<Vec<SqlValue>>, String> {
    db.query_many(sql, &[])
}

pub fn lire(db: &dyn DbBackend) -> Result<Repartitions, String> {
    let e = db.engine();

    let by_codec = lignes(
        db,
        "SELECT NULLIF(LOWER(TRIM(COALESCE(format, ''))), '') AS codec, COUNT(*) \
         FROM tracks GROUP BY 1",
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_string()),
        tracks: entier_de(l.get(1)),
        people: None,
    })
    .collect();

    let frequence = entier(e, "sample_rate");
    let by_sample_rate = lignes(
        db,
        &format!(
            "SELECT NULLIF({frequence}, 0) AS frequence, COUNT(*) \
             FROM tracks GROUP BY 1"
        ),
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_i64()),
        tracks: entier_de(l.get(1)),
        people: None,
    })
    .collect();

    // Division entière : les deux opérandes sont entiers en SQLite comme en
    // PostgreSQL, `1987 / 10 * 10` vaut 1980 des deux côtés.
    let annee = format!(
        "NULLIF(COALESCE(NULLIF({}, 0), NULLIF({}, 0)), 0)",
        entier(e, "t.year"),
        entier(e, "a.year")
    );
    let by_decade = lignes(
        db,
        &format!(
            "SELECT ({annee}) / 10 * 10 AS decennie, COUNT(*) \
             FROM tracks t LEFT JOIN albums a ON a.id = t.album_id \
             GROUP BY 1"
        ),
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_i64()),
        tracks: entier_de(l.get(1)),
        people: None,
    })
    .collect();

    // La personne : `id:<fiche>` ou `nom:<nom en minuscules>`, pour qu'un id
    // et un nom ne se confondent jamais.
    let by_credit_role = lignes(
        db,
        "SELECT NULLIF(LOWER(TRIM(COALESCE(role, ''))), '') AS role, \
                COUNT(DISTINCT track_id), \
                COUNT(DISTINCT CASE WHEN artist_id IS NOT NULL \
                    THEN 'id:' || CAST(artist_id AS TEXT) \
                    ELSE 'nom:' || LOWER(TRIM(artist_name)) END) \
         FROM track_credits GROUP BY 1",
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_string()),
        tracks: entier_de(l.get(1)),
        people: Some(entier_de(l.get(2))),
    })
    .collect();

    let mut r = Repartitions {
        by_codec,
        by_sample_rate,
        by_decade,
        by_credit_role,
    };
    ranger(&mut r);
    Ok(r)
}

/// Le même ordre sur les deux moteurs (SQLite et PostgreSQL ne placent pas
/// NULL au même endroit) : « inconnu » toujours en dernier ; codecs et rôles
/// du plus fréquent au plus rare, fréquences et décennies croissantes.
fn ranger(r: &mut Repartitions) {
    fn par_nombre<V: Ord>(cases: &mut [Case<V>]) {
        cases.sort_by(|a, b| {
            a.value
                .is_none()
                .cmp(&b.value.is_none())
                .then(b.tracks.cmp(&a.tracks))
                .then(a.value.cmp(&b.value))
        });
    }
    fn par_valeur<V: Ord>(cases: &mut [Case<V>]) {
        cases.sort_by(|a, b| {
            a.value
                .is_none()
                .cmp(&b.value.is_none())
                .then(a.value.cmp(&b.value))
        });
    }
    par_nombre(&mut r.by_codec);
    par_nombre(&mut r.by_credit_role);
    par_valeur(&mut r.by_sample_rate);
    par_valeur(&mut r.by_decade);
}
