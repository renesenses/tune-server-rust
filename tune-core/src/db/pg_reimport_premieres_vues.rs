//! Réimport des dates d'ajout (`file_first_seen`) depuis l'ancienne base
//! SQLite, pour une base PostgreSQL DÉJÀ basculée (#5389).
//!
//! Jusqu'à #5389, la bascule SQLite → PostgreSQL ne copiait pas
//! `file_first_seen`. En PostgreSQL, chaque piste retombait donc sur son
//! `file_mtime`, et le scan complet suivant figeait ce mtime comme date d'ajout
//! (#4546). Corriger la bascule ne répare pas une base déjà basculée. Rejouer
//! la bascule non plus : sa copie fait `ON CONFLICT DO NOTHING` et ne réécrit
//! donc jamais une ligne existante.
//!
//! Cet outil relit le `file_first_seen` de l'ancienne base SQLite et **écrase**
//! la date PostgreSQL de chaque chemin présent dans les DEUX bases. Il ne fait
//! rien d'autre :
//! - aucun chemin n'est ajouté (un chemin absent de PostgreSQL est compté,
//!   puis laissé) ;
//! - un chemin présent seulement en PostgreSQL n'est pas touché ;
//! - aucune autre table n'est écrite ;
//! - la base SQLite est ouverte en LECTURE SEULE.
//!
//! Tout se fait dans une seule transaction : soit tout est réécrit, soit rien.
//! Une seconde passe ne change plus rien, car seules les dates différentes
//! sont réécrites.

use std::path::Path;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tracing::info;

/// Le compte rendu d'un réimport.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReimportPremieresVues {
    /// Lignes lues dans le `file_first_seen` SQLite.
    pub lues: usize,
    /// Dates PostgreSQL remplacées par la date SQLite.
    pub ecrasees: usize,
    /// Chemins déjà à la bonne date en PostgreSQL : rien à écrire.
    pub deja_justes: usize,
    /// Chemins absents du `file_first_seen` PostgreSQL : laissés, jamais
    /// ajoutés.
    pub absentes_en_pg: usize,
    /// Lignes SQLite dont la date n'est pas un nombre : laissées.
    pub illisibles: usize,
}

/// Taille des lots envoyés à PostgreSQL.
const LOT: usize = 1000;

/// Lit le `file_first_seen` de `sqlite_path` (en lecture seule).
fn lire_sqlite(sqlite_path: &Path) -> Result<(Vec<(String, f64)>, usize), String> {
    if !sqlite_path.is_file() {
        return Err(format!(
            "ancienne base SQLite introuvable : {}",
            sqlite_path.display()
        ));
    }
    let conn = rusqlite::Connection::open_with_flags(
        sqlite_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("ouverture de {} : {e}", sqlite_path.display()))?;
    let mut stmt = conn
        .prepare("SELECT file_path, first_seen_at FROM file_first_seen")
        .map_err(|e| format!("lecture de file_first_seen (SQLite) : {e}"))?;
    let mut lignes = Vec::new();
    let mut illisibles = 0;
    let mut rangs = stmt
        .query([])
        .map_err(|e| format!("lecture de file_first_seen (SQLite) : {e}"))?;
    while let Some(r) = rangs
        .next()
        .map_err(|e| format!("lecture de file_first_seen (SQLite) : {e}"))?
    {
        let chemin: Option<String> = r.get(0).ok();
        // `f64` accepte un REAL comme un INTEGER ; un texte est illisible.
        let date: Option<f64> = r.get(1).ok();
        match (chemin, date) {
            (Some(c), Some(d)) if d.is_finite() => lignes.push((c, d)),
            _ => illisibles += 1,
        }
    }
    Ok((lignes, illisibles))
}

/// Réimporte les dates d'ajout de `sqlite_path` dans la base PostgreSQL
/// `pg_url`, en écrasant celles des chemins présents des deux côtés.
pub async fn reimporter_premieres_vues(
    sqlite_path: &Path,
    pg_url: &str,
) -> Result<ReimportPremieresVues, String> {
    let (lignes, illisibles) = lire_sqlite(sqlite_path)?;
    let mut compte = ReimportPremieresVues {
        lues: lignes.len() + illisibles,
        ecrasees: 0,
        deja_justes: 0,
        absentes_en_pg: 0,
        illisibles,
    };

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(15))
        .connect(pg_url)
        .await
        .map_err(|e| format!("pg connect: {e}"))?;
    let mut tx = pool.begin().await.map_err(|e| format!("pg begin: {e}"))?;

    for lot in lignes.chunks(LOT) {
        let chemins: Vec<&str> = lot.iter().map(|(c, _)| c.as_str()).collect();
        let dates: Vec<f64> = lot.iter().map(|(_, d)| *d).collect();
        // Présents en PostgreSQL, et à quelle date : une seule lecture.
        let (presents, justes): (i64, i64) = sqlx::query_as(
            "SELECT count(f.file_path), \
                    count(f.file_path) FILTER (WHERE f.first_seen_at = v.t) \
             FROM unnest($1::text[], $2::float8[]) AS v(p, t) \
             LEFT JOIN file_first_seen f ON f.file_path = v.p",
        )
        .bind(&chemins)
        .bind(&dates)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("lecture de file_first_seen (PostgreSQL) : {e}"))?;
        let ecrites = sqlx::query(
            "UPDATE file_first_seen AS f SET first_seen_at = v.t \
             FROM unnest($1::text[], $2::float8[]) AS v(p, t) \
             WHERE f.file_path = v.p AND f.first_seen_at IS DISTINCT FROM v.t",
        )
        .bind(&chemins)
        .bind(&dates)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("écriture de file_first_seen (PostgreSQL) : {e}"))?
        .rows_affected() as usize;
        compte.ecrasees += ecrites;
        compte.deja_justes += justes as usize;
        compte.absentes_en_pg += lot.len() - presents as usize;
    }

    tx.commit().await.map_err(|e| format!("pg commit: {e}"))?;
    pool.close().await;
    info!(
        lues = compte.lues,
        ecrasees = compte.ecrasees,
        deja_justes = compte.deja_justes,
        absentes_en_pg = compte.absentes_en_pg,
        illisibles = compte.illisibles,
        "reimport_premieres_vues_termine"
    );
    Ok(compte)
}

/// Sautés sans `TUNE_TEST_PG_URL` ; l'étape « Bascule SQLite -> PostgreSQL,
/// clefs de conflit » de `test-postgres.yml` les exécute (filtre
/// `pg_bascule_`).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pg_migrate::tests::{base_jetable, supprimer_base};
    use sqlx::{Connection, PgConnection, Row};

    async fn dates_pg(url: &str) -> Vec<(String, f64)> {
        let mut c = PgConnection::connect(url).await.unwrap();
        let v = sqlx::query("SELECT file_path, first_seen_at FROM file_first_seen ORDER BY 1")
            .fetch_all(&mut c)
            .await
            .unwrap()
            .iter()
            .map(|l| (l.get::<String, _>(0), l.get::<f64, _>(1)))
            .collect();
        c.close().await.ok();
        v
    }

    /// Une date figée au mtime est écrasée par la vraie. Un chemin présent
    /// seulement en PostgreSQL n'est pas touché, un chemin présent seulement
    /// en SQLite n'est pas ajouté, et une seconde passe ne change plus rien.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_bascule_reimport_premieres_vues_5389() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const BASE: &str = "tune_reimport_premieres_vues_5389";
        let cible = base_jetable(&url, BASE).await;
        let mut c = PgConnection::connect(&cible).await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE file_first_seen (file_path TEXT PRIMARY KEY, first_seen_at DOUBLE PRECISION NOT NULL);
             INSERT INTO file_first_seen VALUES
                 ('/musique/fige.flac', 1700000000.0),
                 ('/musique/juste.flac', 1400000000.5),
                 ('/musique/pg-seul.flac', 1600000000.0);",
        )
        .execute(&mut c)
        .await
        .unwrap();
        c.close().await.ok();

        let dossier = tempfile::tempdir().unwrap();
        let sqlite = dossier.path().join("tune.db");
        rusqlite::Connection::open(&sqlite)
            .unwrap()
            .execute_batch(
                "CREATE TABLE file_first_seen (file_path TEXT PRIMARY KEY, first_seen_at REAL NOT NULL);
                 INSERT INTO file_first_seen VALUES
                     ('/musique/fige.flac', 1316000000.25),
                     ('/musique/juste.flac', 1400000000.5),
                     ('/musique/sqlite-seul.flac', 1200000000.0),
                     ('/musique/illisible.flac', 'hier');",
            )
            .unwrap();

        let premiere = reimporter_premieres_vues(&sqlite, &cible).await;
        let apres_premiere = dates_pg(&cible).await;
        let seconde = reimporter_premieres_vues(&sqlite, &cible).await;
        let apres_seconde = dates_pg(&cible).await;
        supprimer_base(&url, BASE).await;

        let attendu = vec![
            ("/musique/fige.flac".to_string(), 1316000000.25),
            ("/musique/juste.flac".to_string(), 1400000000.5),
            ("/musique/pg-seul.flac".to_string(), 1600000000.0),
        ];
        assert_eq!(
            premiere,
            Ok(ReimportPremieresVues {
                lues: 4,
                ecrasees: 1,
                deja_justes: 1,
                absentes_en_pg: 1,
                illisibles: 1,
            })
        );
        assert_eq!(
            apres_premiere, attendu,
            "la date figée doit être écrasée, et rien d'autre ne doit bouger"
        );
        assert_eq!(
            seconde,
            Ok(ReimportPremieresVues {
                lues: 4,
                ecrasees: 0,
                deja_justes: 2,
                absentes_en_pg: 1,
                illisibles: 1,
            }),
            "une seconde passe ne doit plus rien écrire"
        );
        assert_eq!(apres_seconde, attendu);
    }

    /// Une ancienne base introuvable est un refus, pas une base créée à vide.
    #[tokio::test]
    async fn reimport_refuse_une_base_sqlite_absente() {
        let dossier = tempfile::tempdir().unwrap();
        let absente = dossier.path().join("absente.db");
        let r = reimporter_premieres_vues(&absente, "postgresql://inutile@127.0.0.1:1/x").await;
        assert!(r.is_err());
        assert!(
            !absente.exists(),
            "l'outil ne doit pas créer la base SQLite"
        );
    }
}
