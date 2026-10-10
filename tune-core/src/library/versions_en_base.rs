//! Les exemplaires d'un enregistrement QUI SONT EN BASE (#2264).
//!
//! Une seule lecture pour la route « Autres versions » groupées
//! (`tune-server/src/routes/library/versions_groupes.rs`) et pour la règle de
//! lecture (`orchestrator/version_de_lecture.rs`) : les deux doivent voir les
//! mêmes candidats pour que la version jouée soit celle que l'écran annonce.
//!
//! Les requêtes par identifiant comparent l'ISRC et le MBID PLIÉS
//! ([`SQL_ISRC_PLIE`], [`SQL_MBID_PLIE`]) : ce sont les expressions des index
//! de la migration 122 (PG 086), et un index d'expression ne sert qu'à
//! l'identique.

use std::sync::Arc;

use super::groupes_versions::{Exemplaire, Qualite};
use super::track_matcher::normaliser_isrc;
use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
pub use crate::db::migrations::{SQL_ISRC_PLIE, SQL_MBID_PLIE};

/// Les colonnes lues pour une piste, dans l'ordre de [`exemplaire_de_ligne`].
/// Alias imposés : `t`, `al`, `ar`, `ar2`. Indices fixes : 12 = `album_id`,
/// 13 = `cover_path`, 14 = `file_path`.
pub const COLONNES_PISTE: &str = "t.id, t.title, COALESCE(ar2.name, ar.name, ''), \
     COALESCE(al.title, ''), t.isrc, t.musicbrainz_recording_id, t.duration_ms, \
     COALESCE(t.source, 'local'), t.source_id, t.format, t.sample_rate, t.bit_depth, \
     t.album_id, al.cover_path, t.file_path";

pub const JOINTURES_PISTE: &str = "FROM tracks t \
     LEFT JOIN albums al ON t.album_id = al.id \
     LEFT JOIN artists ar ON al.artist_id = ar.id \
     LEFT JOIN artists ar2 ON t.artist_id = ar2.id";

/// Indice de `album_id` dans une ligne de [`COLONNES_PISTE`].
pub const COL_ALBUM_ID: usize = 12;
/// Indice de `cover_path`.
pub const COL_COVER: usize = 13;
/// Indice de `file_path`.
pub const COL_FICHIER: usize = 14;

/// Plafond des pistes rapprochées par identifiant.
pub const PLAFOND_PAR_IDENTIFIANT: i64 = 200;

pub type Ligne = Vec<SqlValue>;

fn marqueur(e: Engine, n: usize) -> String {
    match e {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// L'exemplaire que décrit une ligne de [`COLONNES_PISTE`].
pub fn exemplaire_de_ligne(cols: &Ligne) -> Exemplaire {
    let s = |i: usize| cols.get(i).and_then(|v| v.as_string());
    let n = |i: usize| cols.get(i).and_then(|v| v.as_i64());
    Exemplaire {
        source: s(7).unwrap_or_else(|| "local".into()),
        track_id: n(0),
        source_id: s(8),
        titre: s(1).unwrap_or_default(),
        artiste: s(2).unwrap_or_default(),
        album: s(3).unwrap_or_default(),
        isrc: s(4),
        mbid_enregistrement: s(5),
        duree_ms: n(6),
        qualite: Some(Qualite {
            format: s(9),
            sample_rate: n(10),
            bit_depth: n(11),
        }),
        disponible: None,
    }
}

/// Une piste par son identifiant. `None` : elle n'existe pas.
pub fn lire_piste(db: &Arc<dyn DbBackend>, id: i64) -> Option<(Exemplaire, Ligne)> {
    let sql = format!(
        "SELECT {COLONNES_PISTE} {JOINTURES_PISTE} WHERE t.id = {}",
        marqueur(db.engine(), 1)
    );
    db.query_one(&sql, &[&id as &dyn ToSqlValue])
        .ok()
        .flatten()
        .map(|cols| (exemplaire_de_ligne(&cols), cols))
}

/// L'ISRC et le MBID d'un exemplaire, pliés comme les index les portent.
/// Vides quand inconnus.
pub fn identifiants_plies(e: &Exemplaire) -> (String, String) {
    let isrc = e.isrc.as_deref().map(normaliser_isrc).unwrap_or_default();
    let mbid = e
        .mbid_enregistrement
        .as_deref()
        .map(|m| m.trim().to_ascii_lowercase())
        .unwrap_or_default();
    (isrc, mbid)
}

/// La requête de [`pistes_par_identifiant`] : `sauf` en premier paramètre,
/// puis l'ISRC plié et le MBID plié quand ils sont demandés.
pub fn sql_par_identifiant(e: Engine, avec_isrc: bool, avec_mbid: bool, limite: i64) -> String {
    let mut n = 1;
    let mut termes: Vec<String> = Vec::new();
    if avec_isrc {
        n += 1;
        termes.push(format!("{SQL_ISRC_PLIE} = {}", marqueur(e, n)));
    }
    if avec_mbid {
        n += 1;
        termes.push(format!("{SQL_MBID_PLIE} = {}", marqueur(e, n)));
    }
    if termes.is_empty() {
        termes.push("1 = 0".into());
    }
    format!(
        "SELECT {COLONNES_PISTE} {JOINTURES_PISTE} \
         WHERE t.id <> {} AND ({}) ORDER BY t.id LIMIT {limite}",
        marqueur(e, 1),
        termes.join(" OR ")
    )
}

/// Les pistes qui partagent l'ISRC ou le MBID d'enregistrement de
/// `reference`, quel que soit leur titre, `sauf` exclue.
///
/// Chaque terme n'est posé que si l'identifiant est connu, et compare
/// l'expression pliée À L'IDENTIQUE de l'index : SQLite (optimisation OR) et
/// PostgreSQL (BitmapOr) servent alors chacun par son index. La comparaison
/// exacte est refaite en Rust par `relation` ; ce SQL ne fait que trouver.
pub fn pistes_par_identifiant(
    db: &Arc<dyn DbBackend>,
    reference: &Exemplaire,
    sauf: Option<i64>,
    limite: i64,
) -> Vec<(Exemplaire, Ligne)> {
    let (isrc, mbid) = identifiants_plies(reference);
    if isrc.is_empty() && mbid.is_empty() {
        return Vec::new();
    }
    let sauf = sauf.unwrap_or(-1);
    let mut params: Vec<&dyn ToSqlValue> = vec![&sauf];
    if !isrc.is_empty() {
        params.push(&isrc);
    }
    if !mbid.is_empty() {
        params.push(&mbid);
    }
    let sql = sql_par_identifiant(db.engine(), !isrc.is_empty(), !mbid.is_empty(), limite);
    match db.query_many(&sql, &params) {
        Ok(lignes) => lignes
            .into_iter()
            .map(|c| (exemplaire_de_ligne(&c), c))
            .collect(),
        Err(err) => {
            tracing::warn!(erreur = %err, "versions_pistes_par_identifiant_echec");
            Vec::new()
        }
    }
}

/// Les pistes LOCALES du même artiste dont la durée tombe à ±2 s de celle de
/// `reference` : les candidats du rapprochement heuristique, que `grouper`
/// trie ensuite (noyau du titre, marqueurs d'édition).
///
/// Bornée par l'index de l'artiste (`idx_tracks_artist_id`, et
/// `idx_tracks_album_id` par les albums de l'artiste) : jamais un parcours de
/// `tracks`. L'artiste est désigné par son nom plié ; une référence sans
/// artiste ou sans durée ne rend rien.
pub fn pistes_locales_par_titre(
    db: &Arc<dyn DbBackend>,
    reference: &Exemplaire,
    sauf: Option<i64>,
    limite: i64,
) -> Vec<(Exemplaire, Ligne)> {
    let artiste = reference.artiste.trim().to_lowercase();
    let Some(duree) = reference.duree_ms.filter(|d| *d > 0) else {
        return Vec::new();
    };
    if artiste.is_empty() {
        return Vec::new();
    }
    let tol = super::groupes_versions::TOLERANCE_DUREE_MS as i64;
    let (min, max) = (duree - tol, duree + tol);
    let sauf = sauf.unwrap_or(-1);
    let e = db.engine();
    let sql = format!(
        "SELECT {COLONNES_PISTE} {JOINTURES_PISTE} \
         WHERE t.id IN ( \
           SELECT t2.id FROM tracks t2 WHERE t2.artist_id IN (SELECT id FROM artists WHERE LOWER(name) IN ({m1}, {m2})) \
           UNION \
           SELECT t3.id FROM tracks t3 WHERE t3.album_id IN ( \
             SELECT a.id FROM albums a WHERE a.artist_id IN (SELECT id FROM artists WHERE LOWER(name) IN ({m3}, {m4})))) \
         AND t.id <> {m5} AND COALESCE(t.source, 'local') = 'local' \
         AND t.duration_ms BETWEEN {m6} AND {m7} \
         ORDER BY t.id LIMIT {limite}",
        m1 = marqueur(e, 1),
        m2 = marqueur(e, 2),
        m3 = marqueur(e, 3),
        m4 = marqueur(e, 4),
        m5 = marqueur(e, 5),
        m6 = marqueur(e, 6),
        m7 = marqueur(e, 7),
    );
    // `LOWER` de SQLite ne plie que l'ASCII, celui de PostgreSQL plie tout :
    // les deux formes sont proposées. SQLite numérote ses marqueurs par
    // position, d'où les répétitions.
    let ascii = reference.artiste.trim().to_ascii_lowercase();
    let params: [&dyn ToSqlValue; 7] = [&ascii, &artiste, &ascii, &artiste, &sauf, &min, &max];
    match db.query_many(&sql, &params) {
        Ok(lignes) => lignes
            .into_iter()
            .map(|c| (exemplaire_de_ligne(&c), c))
            .collect(),
        Err(err) => {
            tracing::warn!(erreur = %err, "versions_pistes_locales_par_titre_echec");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn index_de_tracks(db: &Arc<dyn DbBackend>) -> Vec<String> {
        db.query_many(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'tracks' \
             AND name IN ('idx_tracks_isrc_norm', 'idx_tracks_mbid_recording_norm') ORDER BY name",
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect()
    }

    fn plan(db: &Arc<dyn DbBackend>, avec_isrc: bool, avec_mbid: bool) -> String {
        let sql = format!(
            "EXPLAIN QUERY PLAN {}",
            sql_par_identifiant(Engine::Sqlite, avec_isrc, avec_mbid, 200)
        );
        let sauf = -1i64;
        let isrc = "USSM18200001".to_string();
        let mbid = "0b1c".to_string();
        let mut p: Vec<&dyn ToSqlValue> = vec![&sauf];
        if avec_isrc {
            p.push(&isrc);
        }
        if avec_mbid {
            p.push(&mbid);
        }
        db.query_many(&sql, &p)
            .unwrap()
            .iter()
            .filter_map(|r| r.last().and_then(|v| v.as_string()))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn les_index_de_la_migration_122_sont_poses_et_servent_la_requete() {
        let db = base();
        assert_eq!(
            index_de_tracks(&db),
            vec!["idx_tracks_isrc_norm", "idx_tracks_mbid_recording_norm"]
        );
        let p = plan(&db, true, true);
        assert!(
            p.contains("idx_tracks_isrc_norm"),
            "ISRC par son index : {p}"
        );
        assert!(
            p.contains("idx_tracks_mbid_recording_norm"),
            "MBID par son index : {p}"
        );
        let p = plan(&db, true, false);
        assert!(p.contains("idx_tracks_isrc_norm"), "{p}");
        assert!(!p.contains("SCAN t "), "aucun parcours de tracks : {p}");

        // Contre-épreuve : sans les index, la même requête parcourt `tracks`.
        db.execute_batch(
            "DROP INDEX idx_tracks_isrc_norm; DROP INDEX idx_tracks_mbid_recording_norm;",
        )
        .unwrap();
        let p = plan(&db, true, true);
        assert!(p.contains("SCAN t"), "sans index : parcours — {p}");
    }

    #[test]
    fn la_requete_par_identifiant_trouve_l_isrc_ecrit_autrement() {
        let db = base();
        db.execute_batch(
            "INSERT INTO tracks (id, title, isrc, musicbrainz_recording_id) VALUES \
               (1, 'Ref', 'USSM18200001', NULL), \
               (2, 'Tirets', 'us-sm1-82-00001', NULL), \
               (3, 'Mbid', NULL, ' 0B1C-MBID '), \
               (4, 'Autre', 'FRZ120000001', NULL);",
        )
        .unwrap();
        let reference = Exemplaire {
            track_id: Some(1),
            isrc: Some("USSM18200001".into()),
            mbid_enregistrement: Some("0b1c-mbid".into()),
            ..Default::default()
        };
        let ids: Vec<i64> = pistes_par_identifiant(&db, &reference, Some(1), 200)
            .iter()
            .filter_map(|(e, _)| e.track_id)
            .collect();
        assert_eq!(ids, vec![2, 3]);
    }
}
