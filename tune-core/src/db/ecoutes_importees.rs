//! #7763 — les écoutes IMPORTÉES d'une piste : Roon, Plex, liste m3u.
//!
//! Le nombre d'écoutes d'une piste n'est stocké nulle part : il se compte dans
//! `listen_history` (`HistoryRepo::plays_for_tracks`). Reporter « 42 écoutes »
//! depuis un autre logiciel aurait donc voulu dire écrire 42 lignes datées
//! dans l'historique — de fausses écoutes, à des dates que personne n'a
//! vécues, que l'Historique et « Mon écoute » afficheraient comme vraies.
//! L'issue l'exclut.
//!
//! Elles vivent donc à part, une ligne par piste (table `imported_plays`,
//! migration SQLite 136 / PG 101) :
//!
//! * `play_count` : la somme des écoutes importées. Un import AJOUTE, il ne
//!   remplace pas : deux imports de 5 écoutes en font 10.
//! * `last_played_at` : la plus récente des dates importées, au format de
//!   `listen_history.listened_at` (`AAAA-MM-JJTHH:MM:SSZ`). Une date plus
//!   ancienne que celle déjà rangée est ignorée.
//!
//! Le nombre affiché est `COUNT(historique) + play_count`, la dernière écoute
//! affichée la plus récente des deux : une date importée plus ancienne que la
//! dernière vraie écoute ne change donc rien à ce qu'on voit.

use std::collections::HashMap;
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

pub struct EcoutesImporteesRepo {
    db: Arc<dyn DbBackend>,
}

impl EcoutesImporteesRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        EcoutesImporteesRepo { db }
    }

    fn ph(&self, n: usize) -> String {
        match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(n),
            Engine::Postgres => PostgresDialect.placeholder(n),
        }
    }

    /// Ajoute `n` écoutes à la piste `track_id`, et sa dernière écoute si
    /// `derniere` (déjà normalisée par [`date_normalisee`]) est plus récente
    /// que celle rangée. `n <= 0` sans date : rien à écrire.
    pub fn ajouter(&self, track_id: i64, n: i64, derniere: Option<&str>) -> Result<(), String> {
        let n = n.max(0);
        if n == 0 && derniere.is_none() {
            return Ok(());
        }
        let sql = format!(
            "INSERT INTO imported_plays (track_id, play_count, last_played_at) \
             VALUES ({}, {}, {}) \
             ON CONFLICT (track_id) DO UPDATE SET \
             play_count = imported_plays.play_count + excluded.play_count, \
             last_played_at = CASE \
                 WHEN excluded.last_played_at IS NOT NULL \
                  AND (imported_plays.last_played_at IS NULL \
                       OR excluded.last_played_at > imported_plays.last_played_at) \
                 THEN excluded.last_played_at \
                 ELSE imported_plays.last_played_at END",
            self.ph(1),
            self.ph(2),
            self.ph(3)
        );
        self.db.execute(&sql, &[&track_id, &n, &derniere])?;
        Ok(())
    }

    /// Pose la note importée (1 à 5) sur la piste, si elle diffère de la
    /// sienne. Rend `true` si la note a changé.
    ///
    /// C'est `tracks.rating`, la note que la bibliothèque affiche et trie.
    /// Limite connue : un rescan d'un fichier qui porte sa propre note dans
    /// ses balises la remet (`COALESCE(nouvelle, rating)` du scanner).
    pub fn reporter_note(&self, track_id: i64, note: i32) -> Result<bool, String> {
        let sql = format!(
            "UPDATE tracks SET rating = {} WHERE id = {} \
             AND (rating IS NULL OR rating <> {})",
            self.ph(1),
            self.ph(2),
            self.ph(3)
        );
        Ok(self.db.execute(&sql, &[&note, &track_id, &note])? > 0)
    }

    /// `(écoutes importées, dernière date importée)` pour chaque piste qui en
    /// a ; une piste absente n'en a aucune.
    pub fn par_piste(
        &self,
        track_ids: &[i64],
    ) -> Result<HashMap<i64, (i64, Option<String>)>, String> {
        if track_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Que des entiers, lus en base par l'appelant — jamais une chaîne du
        // client (même règle que `plays_for_tracks`).
        let id_list = track_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let rows = self.db.query_many(
            &format!(
                "SELECT track_id, play_count, last_played_at FROM imported_plays \
                 WHERE track_id IN ({id_list})"
            ),
            &[],
        )?;
        let mut map = HashMap::new();
        for cols in rows {
            let Some(id) = cols.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let n = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            let derniere = cols.get(2).and_then(|v| v.as_string());
            map.insert(id, (n, derniere));
        }
        Ok(map)
    }
}

/// Une date d'export ramenée au format de `listen_history.listened_at`
/// (`AAAA-MM-JJTHH:MM:SSZ`, UTC), pour que les deux se comparent comme des
/// chaînes. Reconnaît : un horodatage Unix en secondes (Plex `lastViewedAt`),
/// RFC 3339, `AAAA-MM-JJ HH:MM:SS` (pris en UTC) et `AAAA-MM-JJ`. Toute autre
/// forme rend `None` : une date douteuse n'est pas reportée.
pub fn date_normalisee(brute: &str) -> Option<String> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
    const FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
    let s = brute.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(secondes) = s.parse::<i64>() {
        // 0 ou négatif : « jamais », pas le 1er janvier 1970.
        if secondes <= 0 {
            return None;
        }
        return DateTime::<Utc>::from_timestamp(secondes, 0).map(|d| d.format(FORMAT).to_string());
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc).format(FORMAT).to_string());
    }
    for motif in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, motif) {
            return Some(d.and_utc().format(FORMAT).to_string());
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().format(FORMAT).to_string())
}

#[cfg(test)]
#[path = "ecoutes_importees_tests.rs"]
mod tests;
