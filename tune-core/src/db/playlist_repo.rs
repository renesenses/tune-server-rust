use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for playlist_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO playlists (name, description, profile_id) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p WHERE p.id = {}",
            d.placeholder(1)
        )
    }

    /// Same projection as [`get_by_id`], but the row must ALSO belong to the
    /// asking profile. Playlist ids are small sequential integers, so `WHERE id
    /// = ?` alone lets any caller walk the whole household's playlists (#2794).
    /// The ownership test belongs in the statement, not in a prior read: a
    /// check-then-act pair can be raced, and it is one more place to forget.
    pub fn get_by_id_scoped<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p WHERE p.id = {} AND p.profile_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn list<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT p.id, p.name, p.description, (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) FROM playlists p WHERE p.profile_id = {} ORDER BY LOWER(p.name) LIMIT {} OFFSET {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    /// Pochettes des playlists d'UNE PAGE, en une seule requête.
    ///
    /// Le client construisait la mosaïque en interrogeant les pistes de chaque
    /// playlist : une requête HTTP par playlist. Invisible sur treize en réseau
    /// local, intenable sur cent depuis l'extérieur.
    ///
    /// La sous-requête `IN` reprend EXACTEMENT la pagination de [`list`] —
    /// mêmes trois paramètres, même tri. Elle rend donc les pochettes des
    /// playlists de cette page, et d'aucune autre : pas de second passage sur
    /// toute la table.
    ///
    /// `MIN(pt.position)` fixe l'ordre : une pochette apparaît à la place de sa
    /// PREMIÈRE piste dans la playlist. Sans lui, deux pochettes à égalité
    /// sortiraient dans un ordre laissé au moteur, et la mosaïque changerait
    /// d'un rafraîchissement à l'autre.
    ///
    /// `COALESCE(t.cover_path, al.cover_path)` : la pochette de la PISTE, et à
    /// défaut celle de son ALBUM. Bertrand a formulé la règle comme « les 4
    /// premières distinctes parmi les ALBUMS » (02/09/2026), et un fichier peut
    /// n'embarquer aucune pochette alors que son album en a une. Sans ce repli,
    /// une piste nue ferait perdre une case — silencieusement, puisque la
    /// mosaïque afficherait simplement une image de moins.
    ///
    /// Mesuré sur le serveur de test : 135 pistes, AUCUNE sans pochette, donc
    /// aucun changement visible là-bas. Le repli protège les bibliothèques
    /// moins régulières, pas celle qui a servi à écrire ceci.
    ///
    /// Le `GROUP BY` porte sur le TITRE de l'album, insensible à la casse, et
    /// non sur le chemin de la pochette.
    ///
    /// Grouper sur le chemin ne suffisait pas : un même disque est stocké comme
    /// PLUSIEURS lignes d'`albums`, une par artiste crédité, chacune avec son
    /// propre fichier de pochette en cache. Autant de chemins, une seule image.
    ///
    /// Sur la bibliothèque de Bertrand (02/09/2026) : « Les indispensables du
    /// piano » compte treize lignes — treize pianistes —, « I Give It A Year »
    /// quatorze, le coffret Górecki « A Nonesuch Retrospective » quatre.
    ///
    /// 🔴 Le titre SEUL, sans l'artiste. Une première version groupait sur
    /// artiste + titre : c'était viser à côté, puisque l'artiste est justement
    /// ce qui varie d'une ligne à l'autre.
    ///
    /// Le titre du disque ne réunit pas tout : « … (24bit) » et sa version sans
    /// suffixe restent deux groupes. C'est
    /// [`tune_core::library::mosaique::cle_pochette`], côté appelant, qui les
    /// rapproche — un nettoyage que ni SQLite ni Postgres ne savent faire de la
    /// même façon.
    ///
    /// 🔴 Sans titre d'album, la clé retombe sur le CHEMIN. Les pistes hors
    /// album se seraient sinon regroupées sous la clé vide, et une playlist de
    /// titres épars n'aurait montré qu'une seule pochette. Deux tests
    /// existants l'ont attrapé dès la première version de ce groupement.
    pub fn covers_for_page<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT pt.playlist_id, MIN(COALESCE(t.cover_path, al.cover_path)) AS cover, \
             MIN(COALESCE(al.title, '')) AS titre, MIN(pt.position) AS pos \
             FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id \
             LEFT JOIN albums al ON al.id = t.album_id \
             WHERE COALESCE(t.cover_path, al.cover_path) IS NOT NULL \
             AND COALESCE(t.cover_path, al.cover_path) <> '' \
             AND pt.playlist_id IN (SELECT p.id FROM playlists p WHERE p.profile_id = {} \
             ORDER BY LOWER(p.name) LIMIT {} OFFSET {}) \
             GROUP BY pt.playlist_id, \
             LOWER(COALESCE(al.title, t.cover_path, al.cover_path, '')) \
             ORDER BY pt.playlist_id, pos",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM playlists WHERE id = {}", d.placeholder(1))
    }

    pub fn delete_scoped<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM playlists WHERE id = {} AND profile_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_field<D: SqlDialect>(d: &D, field: &str) -> String {
        format!(
            "UPDATE playlists SET {field} = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_field_scoped<D: SqlDialect>(d: &D, field: &str) -> String {
        format!(
            "UPDATE playlists SET {field} = {} WHERE id = {} AND profile_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn max_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM playlist_tracks WHERE playlist_id = {}",
            d.placeholder(1)
        )
    }

    pub fn insert_playlist_track<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete_track_at_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM playlist_tracks WHERE playlist_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_track_ids<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = {} ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn delete_all_tracks<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM playlist_tracks WHERE playlist_id = {}",
            d.placeholder(1)
        )
    }

    pub fn count<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM playlists WHERE profile_id = {}",
            d.placeholder(1)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub id: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub track_count: i64,
}

pub struct PlaylistRepo {
    db: Arc<dyn DbBackend>,
}

impl PlaylistRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    pub fn create(
        &self,
        name: &str,
        description: Option<&str>,
        profile_id: i64,
    ) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 3] = [&name, &description, &profile_id];
        Ok(self.db.execute_returning_id(&sql, &params)?)
    }

    pub fn get(&self, id: i64) -> Result<Option<Playlist>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_playlist))
    }

    /// Read a playlist **only if it belongs to `profile_id`**.
    ///
    /// [`get`] is kept for the internal paths that legitimately have no caller
    /// identity (scan-sync of folder playlists, the public share-token route).
    /// Every HTTP handler that acts on behalf of somebody must use this one:
    /// `WHERE id = ?` alone is not an access control, since the ids are
    /// sequential (#2794).
    pub fn get_for_profile(&self, id: i64, profile_id: i64) -> Result<Option<Playlist>, String> {
        let sql = self.dialect_sql(sql::get_by_id_scoped, sql::get_by_id_scoped);
        let params: [&dyn ToSqlValue; 2] = [&id, &profile_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_playlist))
    }

    pub fn list(&self, profile_id: i64, limit: i64, offset: i64) -> Result<Vec<Playlist>, String> {
        let sql = self.dialect_sql(sql::list, sql::list);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_playlist).collect())
    }

    /// Pochettes par playlist, pour la même page que [`list`].
    ///
    /// Rend au plus `max` pochettes par playlist, dans l'ordre d'apparition.
    /// Une playlist sans aucune pochette est simplement absente de la carte :
    /// l'appelant retombe alors sur son affichage par défaut.
    pub fn covers_for_page(
        &self,
        profile_id: i64,
        limit: i64,
        offset: i64,
        max: usize,
    ) -> Result<std::collections::HashMap<i64, Vec<String>>, String> {
        let sql = self.dialect_sql(sql::covers_for_page, sql::covers_for_page);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        // (clé de mosaïque, chemin) le temps du parcours ; la clé est jetée à la
        // sortie, l'appelant ne veut que les chemins.
        let mut out: std::collections::HashMap<i64, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for cols in &rows {
            let Some(pid) = cols.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let Some(cover) = cols.get(1).and_then(|v| v.as_string()) else {
                continue;
            };
            let titre = cols.get(2).and_then(|v| v.as_string());
            // Le tri de la requête garantit l'ordre.
            //
            // Second dédoublonnage, côté Rust, parce que le `GROUP BY` s'arrête
            // au titre BRUT : « A Nonesuch Retrospective » et « A Nonesuch
            // Retrospective (24bit) » en sortent séparés, alors que c'est la
            // même pochette. `cle_pochette` retire le suffixe.
            let cle = crate::library::mosaique::cle_pochette(titre.as_deref(), &cover);
            let e = out.entry(pid).or_default();
            if e.len() >= max {
                continue;
            }
            // Et sur le CHEMIN : deux albums réellement distincts peuvent
            // partager une pochette (une compilation et sa réédition), et la
            // mosaïque montrerait alors deux fois la même image.
            if e.iter().any(|(_, c)| c == &cover) || e.iter().any(|(k, _)| k == &cle) {
                continue;
            }
            e.push((cle, cover));
        }
        Ok(out
            .into_iter()
            .map(|(pid, v)| (pid, v.into_iter().map(|(_, c)| c).collect()))
            .collect())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update(
        &self,
        id: i64,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), String> {
        if let Some(n) = name {
            let sql = self.dialect_sql(
                |d| sql::update_field(d, "name"),
                |d| sql::update_field(d, "name"),
            );
            let params: [&dyn ToSqlValue; 2] = [&n, &id];
            self.db.execute(&sql, &params)?;
        }
        if let Some(d) = description {
            let sql = self.dialect_sql(
                |dlc| sql::update_field(dlc, "description"),
                |dlc| sql::update_field(dlc, "description"),
            );
            let params: [&dyn ToSqlValue; 2] = [&d, &id];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    /// Delete a playlist **only if it belongs to `profile_id`**. Returns
    /// whether a row was actually removed, so the caller answers `404` instead
    /// of a `204` that deleted nothing — the silent no-op is exactly how a
    /// missing access control stays invisible.
    pub fn delete_for_profile(&self, id: i64, profile_id: i64) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::delete_scoped, sql::delete_scoped);
        let params: [&dyn ToSqlValue; 2] = [&id, &profile_id];
        Ok(self.db.execute(&sql, &params)? > 0)
    }

    /// Update a playlist **only if it belongs to `profile_id`**. Returns
    /// whether the playlist was reachable by that profile at all — a body with
    /// neither field still answers that question, by reading the row back.
    pub fn update_for_profile(
        &self,
        id: i64,
        profile_id: i64,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, String> {
        if name.is_none() && description.is_none() {
            return Ok(self.get_for_profile(id, profile_id)?.is_some());
        }
        let mut touched = false;
        if let Some(n) = name {
            let sql = self.dialect_sql(
                |d| sql::update_field_scoped(d, "name"),
                |d| sql::update_field_scoped(d, "name"),
            );
            let params: [&dyn ToSqlValue; 3] = [&n, &id, &profile_id];
            touched |= self.db.execute(&sql, &params)? > 0;
        }
        if let Some(d) = description {
            let sql = self.dialect_sql(
                |dlc| sql::update_field_scoped(dlc, "description"),
                |dlc| sql::update_field_scoped(dlc, "description"),
            );
            let params: [&dyn ToSqlValue; 3] = [&d, &id, &profile_id];
            touched |= self.db.execute(&sql, &params)? > 0;
        }
        Ok(touched)
    }

    pub fn add_tracks(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<Vec<i64>, String> {
        let max_pos_sql = self.dialect_sql(sql::max_position, sql::max_position);
        let insert_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        let mut inserted = Vec::with_capacity(track_ids.len());
        let inserted_ref = &mut inserted;
        self.db.write_tx(&mut |tx| {
            let max_pos_params: [&dyn ToSqlValue; 1] = [&playlist_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &max_pos_params)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let start_pos = position.unwrap_or(max_pos + 1);
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = start_pos + i as i64;
                let p: [&dyn ToSqlValue; 3] = [&playlist_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
                inserted_ref.push(*tid);
            }
            Ok(())
        })?;
        Ok(inserted)
    }

    /// Like `add_tracks` but skips tracks already in the playlist and repeats
    /// within the batch, so a playlist never holds the same track twice. This
    /// is the path for user "add to playlist" actions (duplicates also made
    /// "remove" look broken — removing one position left the other copy behind,
    /// Elie). Raw `add_tracks` is kept for flows that intentionally preserve
    /// duplicates (e.g. merge-without-dedup).
    pub fn add_tracks_deduped(
        &self,
        playlist_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<Vec<i64>, String> {
        let existing: std::collections::HashSet<i64> =
            self.get_track_ids(playlist_id)?.into_iter().collect();
        let mut batch_seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let to_add: Vec<i64> = track_ids
            .iter()
            .copied()
            .filter(|tid| !existing.contains(tid) && batch_seen.insert(*tid))
            .collect();
        if to_add.is_empty() {
            return Ok(Vec::new());
        }
        self.add_tracks(playlist_id, &to_add, position)
    }

    /// Create a playlist AND fill it, in ONE transaction: either the playlist
    /// exists with its tracks, or nothing was written at all.
    ///
    /// The two-step `create()` + `add_tracks…()` shape used by duplication and
    /// by the playlist imports could not be honest: when the second step
    /// failed, the empty playlist stayed in the database and the route dropped
    /// the error with `.ok()`, so the caller got `201 Created` for a playlist
    /// that holds nothing (#2798). Here a failed track insert rolls the
    /// playlist row back with it.
    ///
    /// `track_ids` is de-duplicated in order (first occurrence wins), like
    /// `add_tracks_deduped` — the playlist is brand new, so there is nothing
    /// else to dedup against. Returns the new id **and the ids actually
    /// written**, so the caller reports what is persisted instead of what it
    /// hoped to persist.
    pub fn create_with_tracks(
        &self,
        name: &str,
        description: Option<&str>,
        profile_id: i64,
        track_ids: &[i64],
    ) -> Result<(i64, Vec<i64>), String> {
        let create_sql = self.dialect_sql(sql::create, sql::create);
        let insert_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);

        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let to_add: Vec<i64> = track_ids
            .iter()
            .copied()
            .filter(|tid| seen.insert(*tid))
            .collect();

        let mut new_id = 0i64;
        {
            let new_id_ref = &mut new_id;
            let to_add_ref = &to_add;
            self.db.write_tx(&mut |tx| {
                let cp: [&dyn ToSqlValue; 3] = [&name, &description, &profile_id];
                tx.execute(&create_sql, &cp)?;
                let id = tx.last_insert_rowid();
                *new_id_ref = id;
                for (i, tid) in to_add_ref.iter().enumerate() {
                    let pos = i as i64;
                    let p: [&dyn ToSqlValue; 3] = [&id, tid, &pos];
                    tx.execute(&insert_sql, &p)?;
                }
                Ok(())
            })?;
        }
        Ok((new_id, to_add))
    }

    pub fn remove_tracks_at_positions(
        &self,
        playlist_id: i64,
        positions: &[i64],
    ) -> Result<usize, String> {
        let delete_sql =
            self.dialect_sql(sql::delete_track_at_position, sql::delete_track_at_position);
        let mut removed = 0usize;
        let removed_ref = &mut removed;
        self.db.write_tx(&mut |tx| {
            for pos in positions {
                let p: [&dyn ToSqlValue; 2] = [&playlist_id, pos];
                *removed_ref += tx.execute(&delete_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(removed)
    }

    pub fn remove_track(&self, playlist_id: i64, position: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_track_at_position, sql::delete_track_at_position);
        let params: [&dyn ToSqlValue; 2] = [&playlist_id, &position];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Replace the whole playlist contents with `track_ids`, in order,
    /// atomically. This is the folder→playlist scan-sync path: the playlist
    /// mirrors its source directory on every scan, so the operation must be
    /// idempotent and never leave a half-replaced list on failure.
    pub fn set_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let delete_sql = self.dialect_sql(sql::delete_all_tracks, sql::delete_all_tracks);
        let insert_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        self.db.write_tx(&mut |tx| {
            let dp: [&dyn ToSqlValue; 1] = [&playlist_id];
            tx.execute(&delete_sql, &dp)?;
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 3] = [&playlist_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn get_track_ids(&self, playlist_id: i64) -> Result<Vec<i64>, String> {
        let sql = self.dialect_sql(sql::get_track_ids, sql::get_track_ids);
        let params: [&dyn ToSqlValue; 1] = [&playlist_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    pub fn reorder_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let delete_sql = self.dialect_sql(sql::delete_all_tracks, sql::delete_all_tracks);
        let insert_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&playlist_id];
            tx.execute(&delete_sql, &p)?;
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 3] = [&playlist_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn count(&self, profile_id: i64) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::count, sql::count);
        let params: [&dyn ToSqlValue; 1] = [&profile_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }
}

fn row_to_playlist(cols: &Vec<SqlValue>) -> Playlist {
    Playlist {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        description: cols.get(2).and_then(|v| v.as_string()),
        track_count: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Track as TrackModel;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    /// Une piste avec la pochette voulue.
    fn piste(
        track_repo: &crate::db::track_repo::TrackRepo,
        titre: &str,
        cover: Option<&str>,
    ) -> i64 {
        let mut t = TrackModel::new(titre.into());
        t.file_path = Some(format!("/{titre}.flac"));
        t.cover_path = cover.map(|c| c.to_string());
        track_repo.create(&t).unwrap()
    }

    /// Les pochettes de la mosaïque : DISTINCTES, dans l'ordre, plafonnées.
    ///
    /// Le client construisait cette liste en interrogeant les pistes de CHAQUE
    /// playlist — une requête HTTP par playlist. Elle vient désormais avec la
    /// liste, en une seule requête.
    #[test]
    fn pochettes_distinctes_dans_l_ordre_et_plafonnees() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        // A, A, B, C, D, E : six pistes, cinq pochettes distinctes.
        // On attend A B C D — la première occurrence fixe le rang, et le
        // doublon de A ne consomme pas une seconde case.
        let ids: Vec<i64> = [
            ("t1", Some("A")),
            ("t2", Some("A")),
            ("t3", Some("B")),
            ("t4", Some("C")),
            ("t5", Some("D")),
            ("t6", Some("E")),
        ]
        .iter()
        .map(|(n, c)| piste(&track_repo, n, *c))
        .collect();

        let pl = repo.create("Melange", None, 1).unwrap();
        repo.add_tracks(pl, &ids, None).unwrap();

        let map = repo.covers_for_page(1, 50, 0, 4).unwrap();
        assert_eq!(
            map.get(&pl).cloned().unwrap_or_default(),
            vec!["A", "B", "C", "D"],
            "ordre d'apparition, doublons ecartes, plafond a quatre"
        );
    }

    /// Une piste sans pochette prend celle de son ALBUM.
    ///
    /// Regle de Bertrand : « les 4 premieres distinctes parmi les ALBUMS ». Un
    /// fichier peut n'embarquer aucune pochette alors que son album en a une —
    /// sans repli, cette piste ferait perdre une case, et en silence : la
    /// mosaique afficherait simplement une image de moins.
    #[test]
    fn une_piste_nue_prend_la_pochette_de_son_album() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db.clone());

        // L'album 1 porte « ALB » ; sa piste, elle, n'a aucune pochette.
        db.execute_batch("INSERT INTO albums (id, title, cover_path) VALUES (1,'Album','ALB');")
            .unwrap();
        let mut nue = TrackModel::new("nue".into());
        nue.file_path = Some("/nue.flac".into());
        nue.album_id = Some(1);
        nue.cover_path = None;
        let id_nue = track_repo.create(&nue).unwrap();

        let id_avec = piste(&track_repo, "avec", Some("PISTE"));

        let pl = repo.create("Melange", None, 1).unwrap();
        repo.add_tracks(pl, &[id_nue, id_avec], None).unwrap();

        assert_eq!(
            repo.covers_for_page(1, 50, 0, 4)
                .unwrap()
                .get(&pl)
                .cloned()
                .unwrap_or_default(),
            vec!["ALB", "PISTE"],
            "la piste nue doit apporter la pochette de son album, en premiere position"
        );
    }

    /// Les albums SANS pochette ne consomment pas de case.
    ///
    /// Regle de Bertrand : quatre pochettes DISTINCTES « si possible »
    /// (02/09/2026). Un fichier nu ne doit donc pas voler une place a une
    /// pochette qui existe plus loin — sinon la mosaique en montrerait trois la
    /// ou la playlist en a quatre, et rien ne le signalerait : elle cyclerait,
    /// ce qui reste credible a l'oeil.
    #[test]
    fn les_pistes_sans_pochette_ne_volent_pas_de_case() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        // Trois pistes nues intercalees entre les pochettes : elles ne doivent
        // ni apparaitre, ni empecher D d'etre atteinte.
        let ids: Vec<i64> = [
            ("n1", None),
            ("p1", Some("A")),
            ("n2", None),
            ("p2", Some("B")),
            ("n3", None),
            ("p3", Some("C")),
            ("p4", Some("D")),
        ]
        .iter()
        .map(|(n, c)| piste(&track_repo, n, *c))
        .collect();

        let pl = repo.create("Troue", None, 1).unwrap();
        repo.add_tracks(pl, &ids, None).unwrap();

        assert_eq!(
            repo.covers_for_page(1, 50, 0, 4)
                .unwrap()
                .get(&pl)
                .cloned()
                .unwrap_or_default(),
            vec!["A", "B", "C", "D"],
            "quatre pochettes distinctes malgre trois pistes nues intercalees"
        );
    }

    /// Un COFFRET ne remplit pas la mosaique a lui seul.
    ///
    /// Un coffret est stocke comme PLUSIEURS albums, chacun avec son propre
    /// fichier de pochette en cache : quatre chemins differents, une seule
    /// image. Groupe sur le chemin, les quatre disques prenaient les quatre
    /// cases. Vecu sur la collection « Classique » de Bertrand — le coffret
    /// Gorecki « A Nonesuch Retrospective » (02/09/2026).
    ///
    /// Le groupe porte donc sur l'ALBUM : artiste + titre, insensibles a la
    /// casse.
    #[test]
    fn un_meme_disque_ne_prend_qu_une_case() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db.clone());

        // Un meme disque eclate en QUATRE lignes d'album, une par artiste
        // credite, chacune avec sa propre pochette en cache. Plus sa reedition
        // 24 bits, que le titre brut ne rejoint pas. Puis deux vrais autres
        // albums.
        //
        // Les artistes DIFFERENT, comme en base : c'est le point. Ma premiere
        // version donnait le meme artiste aux quatre lignes, si bien qu'une cle
        // « artiste + titre » — celle qui a laisse passer le defaut chez
        // Bertrand — rendait ce test vert.
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES \
               (1,'Henryk Gorecki'),(2,'Dawn Upshaw'),(3,'Kronos Quartet'), \
               (4,'London Philharmonic Orchestra'),(9,'Autre'); \
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES \
               (1,'A Nonesuch Retrospective',1,'C1'), \
               (2,'A Nonesuch Retrospective',2,'C2'), \
               (3,'a nonesuch retrospective',3,'C3'), \
               (4,'A Nonesuch Retrospective',4,'C4'), \
               (5,'A Nonesuch Retrospective (24bit)',2,'C5'), \
               (6,'Vrai Deux',9,'D'), \
               (7,'Vrai Trois',9,'E');",
        )
        .unwrap();

        let mut ids = Vec::new();
        for (i, album) in [1i64, 2, 3, 4, 5, 6, 7].iter().enumerate() {
            let mut t = TrackModel::new(format!("t{i}"));
            t.file_path = Some(format!("/t{i}.flac"));
            t.album_id = Some(*album);
            t.cover_path = None; // la pochette vient de l'album
            ids.push(track_repo.create(&t).unwrap());
        }
        let pl = repo.create("Coffret", None, 1).unwrap();
        repo.add_tracks(pl, &ids, None).unwrap();

        let obtenu = repo
            .covers_for_page(1, 50, 0, 4)
            .unwrap()
            .get(&pl)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            obtenu.len(),
            3,
            "le disque, ses quatre artistes et sa reedition 24 bits ne doivent \
             compter que pour UNE case : {obtenu:?}"
        );
        assert!(
            obtenu.contains(&"D".to_string()) && obtenu.contains(&"E".to_string()),
            "les deux autres albums doivent y figurer : {obtenu:?}"
        );
    }

    /// Une playlist sans aucune pochette est ABSENTE de la carte.
    ///
    /// Elle n'y figure pas avec une liste vide : l'appelant distingue ainsi
    /// « rien a montrer » de « pas encore charge », et retombe sur son
    /// affichage par defaut sans dessiner un carre vide.
    #[test]
    fn une_playlist_sans_pochette_est_absente() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let sans = piste(&track_repo, "muette", None);
        let pl = repo.create("Sans pochette", None, 1).unwrap();
        repo.add_tracks(pl, &[sans], None).unwrap();

        assert!(
            !repo.covers_for_page(1, 50, 0, 4).unwrap().contains_key(&pl),
            "une playlist sans pochette ne doit pas apparaitre du tout"
        );
    }

    /// La requete ne rend QUE les playlists de la page demandee.
    ///
    /// C'est tout l'interet : sans le `IN` cale sur la pagination, chaque page
    /// balaierait la table entiere des `playlist_tracks`.
    #[test]
    fn seules_les_playlists_de_la_page_sont_lues() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let t = piste(&track_repo, "commune", Some("Z"));

        // Triees par nom : « Alpha » puis « Beta ».
        let a = repo.create("Alpha", None, 1).unwrap();
        let b = repo.create("Beta", None, 1).unwrap();
        repo.add_tracks(a, &[t], None).unwrap();
        repo.add_tracks(b, &[t], None).unwrap();

        let page1 = repo.covers_for_page(1, 1, 0, 4).unwrap();
        assert!(page1.contains_key(&a), "la premiere page doit porter Alpha");
        assert!(!page1.contains_key(&b), "elle ne doit PAS porter Beta");

        let page2 = repo.covers_for_page(1, 1, 1, 4).unwrap();
        assert!(page2.contains_key(&b), "la seconde page doit porter Beta");
        assert!(!page2.contains_key(&a), "elle ne doit PAS porter Alpha");
    }

    /// Le cloisonnement par profil vaut ici comme ailleurs (#2794).
    #[test]
    fn les_pochettes_ne_traversent_pas_les_profils() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let t = piste(&track_repo, "x", Some("A"));

        let autre = repo.create("Chez l autre", None, 2).unwrap();
        repo.add_tracks(autre, &[t], None).unwrap();

        assert!(
            repo.covers_for_page(1, 50, 0, 4).unwrap().is_empty(),
            "le profil 1 ne doit rien voir des playlists du profil 2"
        );
    }

    #[test]
    fn crud_playlist() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        let id = repo.create("My Playlist", Some("Test"), 1).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "My Playlist");
        assert_eq!(pl.track_count, 0);

        repo.update(id, Some("Renamed"), None).unwrap();
        let pl2 = repo.get(id).unwrap().unwrap();
        assert_eq!(pl2.name, "Renamed");

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn playlist_tracks() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("Song A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("Song B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        let plid = repo.create("Test PL", None, 1).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();

        let ids = repo.get_track_ids(plid).unwrap();
        assert_eq!(ids, vec![tid1, tid2]);

        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 2);

        repo.reorder_tracks(plid, &[tid2, tid1]).unwrap();
        let reordered = repo.get_track_ids(plid).unwrap();
        assert_eq!(reordered, vec![tid2, tid1]);
    }

    #[test]
    fn set_tracks_replaces_contents_in_order() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let ids: Vec<i64> = ["/1.flac", "/2.flac", "/3.flac"]
            .iter()
            .map(|p| {
                let mut t = TrackModel::new((*p).into());
                t.file_path = Some((*p).into());
                track_repo.create(&t).unwrap()
            })
            .collect();

        let plid = repo.create("Dossier", Some("Dossier : /x"), 1).unwrap();
        repo.add_tracks(plid, &[ids[0]], None).unwrap();

        repo.set_tracks(plid, &[ids[2], ids[0], ids[1]]).unwrap();
        assert_eq!(
            repo.get_track_ids(plid).unwrap(),
            vec![ids[2], ids[0], ids[1]]
        );

        // Idempotent: same input, same result; empty input empties the list.
        repo.set_tracks(plid, &[ids[2], ids[0], ids[1]]).unwrap();
        assert_eq!(
            repo.get_track_ids(plid).unwrap(),
            vec![ids[2], ids[0], ids[1]]
        );
        repo.set_tracks(plid, &[]).unwrap();
        assert!(repo.get_track_ids(plid).unwrap().is_empty());
    }

    #[test]
    fn playlist_count() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        assert_eq!(repo.count(1).unwrap(), 0);
        repo.create("Playlist 1", None, 1).unwrap();
        repo.create("Playlist 2", None, 1).unwrap();
        assert_eq!(repo.count(1).unwrap(), 2);
    }

    #[test]
    fn playlist_list() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        repo.create("Zebra", None, 1).unwrap();
        repo.create("Alpha", None, 1).unwrap();
        repo.create("Middle", None, 1).unwrap();

        let all = repo.list(1, 100, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "Alpha");
        assert_eq!(all[2].name, "Zebra");
    }

    #[test]
    fn playlist_scoped_by_profile() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        repo.create("P1 only", None, 1).unwrap();
        repo.create("P2 only", None, 2).unwrap();
        repo.create("P2 second", None, 2).unwrap();

        // list + count are scoped to the requesting profile.
        assert_eq!(repo.count(1).unwrap(), 1);
        assert_eq!(repo.count(2).unwrap(), 2);
        let p1 = repo.list(1, 100, 0).unwrap();
        assert_eq!(p1.len(), 1);
        assert_eq!(p1[0].name, "P1 only");
        let p2 = repo.list(2, 100, 0).unwrap();
        assert_eq!(p2.len(), 2);
    }

    /// #2794 — le test ci-dessus ne couvrait que `list` et `count`, c'est-à-dire
    /// exactement les deux seules opérations qui étaient cloisonnées. Les accès
    /// **par id**, eux, ignoraient le profil.
    #[test]
    fn access_by_id_is_scoped_by_profile() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        let id = repo.create("Privee du profil 1", None, 1).unwrap();

        // Lecture
        assert!(repo.get_for_profile(id, 1).unwrap().is_some());
        assert!(
            repo.get_for_profile(id, 2).unwrap().is_none(),
            "le profil 2 a lu la playlist du profil 1"
        );

        // Modification : refusée ET sans effet en base.
        assert!(
            !repo
                .update_for_profile(id, 2, Some("Detournee"), None)
                .unwrap()
        );
        assert_eq!(repo.get(id).unwrap().unwrap().name, "Privee du profil 1");
        // Un corps vide répond quand même « cette playlist ne vous est pas
        // accessible » plutôt que de mentir par un succès.
        assert!(!repo.update_for_profile(id, 2, None, None).unwrap());
        assert!(repo.update_for_profile(id, 1, None, None).unwrap());

        // Suppression : refusée ET la ligne est toujours là.
        assert!(!repo.delete_for_profile(id, 2).unwrap());
        assert!(repo.get(id).unwrap().is_some());

        // Témoin : le propriétaire, lui, modifie et supprime.
        assert!(
            repo.update_for_profile(id, 1, Some("Renommee"), None)
                .unwrap()
        );
        assert_eq!(repo.get(id).unwrap().unwrap().name, "Renommee");
        assert!(repo.delete_for_profile(id, 1).unwrap());
        assert!(repo.get(id).unwrap().is_none());
        // Une seconde suppression n'a plus rien à supprimer : `false`, pas un
        // succès silencieux.
        assert!(!repo.delete_for_profile(id, 1).unwrap());
    }

    #[test]
    fn playlist_list_pagination() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        for i in 0..10 {
            repo.create(&format!("PL {i:02}"), None, 1).unwrap();
        }

        let page1 = repo.list(1, 3, 0).unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = repo.list(1, 3, 3).unwrap();
        assert_eq!(page2.len(), 3);
        assert_ne!(page1[0].name, page2[0].name);
    }

    #[test]
    fn playlist_update_description() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        let id = repo.create("Test", Some("Initial"), 1).unwrap();
        repo.update(id, None, Some("Updated desc")).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "Test");
        assert_eq!(pl.description.as_deref(), Some("Updated desc"));
    }

    #[test]
    fn playlist_add_tracks_at_position() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let mut t3 = TrackModel::new("C".into());
        t3.file_path = Some("/c.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();
        let tid3 = track_repo.create(&t3).unwrap();

        let plid = repo.create("Test", None, 1).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();
        repo.add_tracks(plid, &[tid3], Some(1)).unwrap();

        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 3);
    }

    #[test]
    fn playlist_add_tracks_skips_duplicates() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        let plid = repo.create("Test", None, 1).unwrap();
        // Duplicate within a single batch → inserted once.
        let added = repo
            .add_tracks_deduped(plid, &[tid1, tid1, tid2], None)
            .unwrap();
        assert_eq!(added, vec![tid1, tid2]);
        // Re-adding an existing track → skipped; only the new one lands.
        let added2 = repo.add_tracks_deduped(plid, &[tid1, tid2], None).unwrap();
        assert!(added2.is_empty());
        let pl = repo.get(plid).unwrap().unwrap();
        assert_eq!(pl.track_count, 2);
        assert_eq!(repo.get_track_ids(plid).unwrap(), vec![tid1, tid2]);
    }

    #[test]
    fn playlist_remove_track() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        let plid = repo.create("Test", None, 1).unwrap();
        repo.add_tracks(plid, &[tid1, tid2], None).unwrap();
        repo.remove_track(plid, 0).unwrap();

        let ids = repo.get_track_ids(plid).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], tid2);
    }

    #[test]
    fn playlist_remove_tracks_at_positions() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t1 = TrackModel::new("A".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = TrackModel::new("B".into());
        t2.file_path = Some("/2.flac".into());
        let mut t3 = TrackModel::new("C".into());
        t3.file_path = Some("/3.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();
        let tid3 = track_repo.create(&t3).unwrap();

        let plid = repo.create("Test", None, 1).unwrap();
        repo.add_tracks(plid, &[tid1, tid2, tid3], None).unwrap();
        let removed = repo.remove_tracks_at_positions(plid, &[0, 2]).unwrap();
        assert_eq!(removed, 2);

        let remaining = repo.get_track_ids(plid).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0], tid2);
    }

    #[test]
    fn playlist_empty_name() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        let id = repo.create("", None, 1).unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "");
    }

    #[test]
    fn playlist_unicode_name() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        let id = repo
            .create("Ma playlist preferee", Some("Musique francaise"), 1)
            .unwrap();
        let pl = repo.get(id).unwrap().unwrap();
        assert_eq!(pl.name, "Ma playlist preferee");
    }

    #[test]
    fn playlist_delete_cascade() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t = TrackModel::new("Track".into());
        t.file_path = Some("/t.flac".into());
        let tid = track_repo.create(&t).unwrap();

        let plid = repo.create("Test", None, 1).unwrap();
        repo.add_tracks(plid, &[tid], None).unwrap();
        repo.delete(plid).unwrap();

        assert!(repo.get(plid).unwrap().is_none());
    }

    #[test]
    fn get_nonexistent_playlist() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create(&s).contains("VALUES (?, ?, ?)"));
        assert!(sql::create(&p).contains("VALUES ($1, $2, $3)"));
        assert!(sql::create(&s).contains("profile_id"));
        assert!(!sql::list(&p).contains("COLLATE"));
        assert!(sql::list(&p).contains("LOWER(p.name)"));
        assert!(sql::list(&p).contains("profile_id ="));
    }

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = PlaylistRepo::with_backend(backend);
        let id = repo.create("X", None, 1).unwrap();
        assert!(repo.get(id).unwrap().is_some());
    }

    // --- #2798 : création + remplissage, tout ou rien -------------------

    /// Ce que `create_with_tracks` rend décrit ce qui est EN BASE : les
    /// positions sont contiguës et les répétitions du lot sont écartées, donc
    /// le nombre rendu ne peut pas dépasser le nombre de lignes écrites.
    #[test]
    fn create_with_tracks_persiste_exactement_ce_qu_il_annonce() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut ids = Vec::new();
        for i in 0..3 {
            let mut t = TrackModel::new(format!("T{i}"));
            t.file_path = Some(format!("/t{i}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }

        // ids[0] apparaît deux fois : un import M3U peut lister deux fois le
        // même fichier.
        let (plid, written) = repo
            .create_with_tracks("Import", None, 1, &[ids[0], ids[1], ids[0], ids[2]])
            .unwrap();

        assert_eq!(written, vec![ids[0], ids[1], ids[2]]);
        assert_eq!(repo.get_track_ids(plid).unwrap(), written);
        assert_eq!(repo.get(plid).unwrap().unwrap().track_count, 3);
    }

    /// Le cœur de #2798 : un échec APRÈS la création de la playlist ne doit
    /// laisser aucune playlist derrière lui.
    ///
    /// L'échec est injecté sans mock : `playlist_tracks.track_id` référence
    /// `tracks(id)` et `PRAGMA foreign_keys=ON`, donc insérer une piste
    /// inexistante échoue — de façon déterministe, sans horloge ni ordre
    /// d'exécution. La deuxième piste est valide : l'échec survient bien au
    /// MILIEU du remplissage, pas au premier insert.
    #[test]
    fn create_with_tracks_ne_laisse_rien_quand_une_piste_echoue() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);

        let mut t = TrackModel::new("Bonne".into());
        t.file_path = Some("/bonne.flac".into());
        let bonne = track_repo.create(&t).unwrap();

        let avant = repo.count(1).unwrap();

        let err = repo
            .create_with_tracks("Copie", None, 1, &[bonne, 999_999_999, bonne + 1])
            .expect_err("un track_id inexistant doit faire échouer la transaction");

        assert_eq!(
            repo.count(1).unwrap(),
            avant,
            "une playlist partielle a survécu à l'échec ({err})"
        );
        assert!(
            repo.list(1, 100, 0)
                .unwrap()
                .iter()
                .all(|p| p.name != "Copie"),
            "la playlist « Copie » est restée en base après l'échec"
        );
    }

    /// Contre-épreuve : l'ancienne séquence (create() puis add_tracks()) laisse
    /// bel et bien la playlist vide derrière elle. Si ce test devenait vert
    /// sans `create_with_tracks`, c'est que l'échec n'est plus injecté et que
    /// le test ci-dessus ne prouve plus rien.
    #[test]
    fn contre_epreuve_l_ancienne_sequence_laisse_une_playlist_orpheline() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);

        let avant = repo.count(1).unwrap();
        let plid = repo.create("Copie ancienne", None, 1).unwrap();
        let echec = repo.add_tracks(plid, &[999_999_999], None);

        assert!(echec.is_err(), "l'échec doit bien être injecté");
        assert_eq!(
            repo.count(1).unwrap(),
            avant + 1,
            "l'ancienne séquence laissait une playlist vide — c'est le défaut #2798"
        );
        assert!(repo.get_track_ids(plid).unwrap().is_empty());
    }
}
