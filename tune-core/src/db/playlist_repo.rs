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

    /// Les pistes LOCALES de la playlist, dans l'ordre. Une ligne de titre de
    /// service (#4889) a `track_id` NUL : elle n'est pas une piste de la
    /// bibliothèque et n'a rien à faire dans cette liste — les appelants
    /// (UPnP, synchronisation, transfert vers un service…) raisonnent sur des
    /// `tracks.id`. [`get_entries`] rend, lui, les deux sortes de lignes.
    pub fn get_track_ids<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = {} AND track_id IS NOT NULL ORDER BY position",
            d.placeholder(1)
        )
    }

    /// Toutes les lignes de la playlist, locales ET de service (#4889), dans
    /// l'ordre d'affichage. `id` départage deux lignes de même `position`
    /// (un ajout à une position explicite ne décale pas les suivantes).
    pub fn get_entries<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, position, track_id, source, source_id, title, artist, album, \
             album_source_id, duration_ms, cover_url \
             FROM playlist_tracks WHERE playlist_id = {} ORDER BY position, id",
            d.placeholder(1)
        )
    }

    /// Une ligne de TITRE DE SERVICE (#4889) : `track_id` reste NUL, le CHECK
    /// de la table l'exige.
    pub fn insert_service_entry<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO playlist_tracks (playlist_id, position, source, source_id, title, \
             artist, album, album_source_id, duration_ms, cover_url) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10)
        )
    }

    pub fn set_entry_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE playlist_tracks SET position = {} WHERE id = {} AND playlist_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
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

    /// Remplace une piste par une autre, À LA MÊME POSITION, dans une seule
    /// playlist (#3685).
    ///
    /// Gardé par l'existence de la piste de remplacement, comme
    /// `insert_local_at_if_exists` dans la file : `playlist_tracks.track_id`
    /// porte `REFERENCES tracks(id)` sur SQLite mais PAS sur PostgreSQL
    /// (`pg_migrate.rs`, `001_initial_schema.sql`) — sans cette garde, un
    /// identifiant périmé serait REFUSÉ sur un moteur et ACCEPTÉ sur l'autre.
    /// Ici les deux rendent `0` ligne.
    pub fn replace_track<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE playlist_tracks SET track_id = {} \
             WHERE playlist_id = {} AND track_id = {} \
             AND EXISTS (SELECT 1 FROM tracks WHERE id = {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn delete_track_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM playlist_tracks WHERE playlist_id = {} AND track_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn contains_track<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT 1 FROM playlist_tracks WHERE playlist_id = {} AND track_id = {} LIMIT 1",
            d.placeholder(1),
            d.placeholder(2)
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

/// Un TITRE DE SERVICE rangé dans une playlist Tune (#4889).
///
/// Ce qu'il faut pour l'AFFICHER et le JOUER sans rappeler le service à
/// chaque liste : l'identité (`source`, `source_id`) et les colonnes
/// d'affichage copiées à l'ajout. Même forme qu'une ligne de service de
/// `queue_items`, pour que la lecture passe par la résolution de streaming
/// existante.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEntry {
    pub source: String,
    pub source_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// L'identifiant de l'album CHEZ LE SERVICE (jamais un `albums.id`).
    pub album_source_id: Option<String>,
    pub duration_ms: Option<i64>,
    pub cover_url: Option<String>,
}

impl ServiceEntry {
    /// La ligne au format que le client connaît déjà pour une piste de
    /// streaming (`StreamTrack` sérialisé + `source`) : `id` nul, `source` /
    /// `source_id`, `artist_name`, `album_title`, `album_id` du service,
    /// `cover_path`. Un client qui sait afficher un résultat Qobuz sait
    /// afficher cette ligne.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": serde_json::Value::Null,
            "title": self.title,
            "artist_name": self.artist,
            "album_title": self.album,
            "album_id": self.album_source_id,
            "duration_ms": self.duration_ms.unwrap_or(0),
            "cover_path": self.cover_url,
            "source": self.source,
            "source_id": self.source_id,
        })
    }

    /// Lit un titre de service tel que le client l'envoie
    /// (`AddToPlaylistModal.buildAddArgs()`, type `StreamingTrackInfo`), en
    /// acceptant aussi les noms de colonnes de la file (`artist`, `album`,
    /// `cover_url`).
    ///
    /// `None` quand il manque de quoi l'identifier OU l'afficher : `source`
    /// (et jamais `local` — une piste locale se désigne par son `tracks.id`),
    /// `source_id`, `title`. La liste ne rappelle pas le service : une ligne
    /// sans titre resterait sans titre pour toujours.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let texte = |cles: &[&str]| -> Option<String> {
            cles.iter().find_map(|k| match v.get(*k)? {
                serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
        };
        let source = texte(&["source"])?;
        if source.eq_ignore_ascii_case("local") {
            return None;
        }
        Some(Self {
            source,
            source_id: texte(&["source_id"])?,
            title: texte(&["title"])?,
            artist: texte(&["artist_name", "artist"]),
            album: texte(&["album_title", "album"]),
            album_source_id: texte(&["album_id_service", "album_id", "album_source_id"]),
            duration_ms: v
                .get("duration_ms")
                .and_then(|d| d.as_i64().or_else(|| d.as_f64().map(|f| f as i64)))
                .filter(|d| *d >= 0),
            cover_url: texte(&["cover_path", "cover_url"]),
        })
    }
}

/// Ce que porte une ligne de playlist : une piste de la bibliothèque, ou un
/// titre de service (#4889). Jamais les deux, jamais aucun — le CHECK de la
/// table le garantit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryContent {
    Local(i64),
    Service(ServiceEntry),
}

impl EntryContent {
    /// La clef de dédoublonnage : un `tracks.id`, ou la paire du service.
    fn cle(&self) -> (Option<i64>, Option<(String, String)>) {
        match self {
            Self::Local(id) => (Some(*id), None),
            Self::Service(s) => (None, Some((s.source.clone(), s.source_id.clone()))),
        }
    }
}

/// Une ligne lue de `playlist_tracks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// `playlist_tracks.id`.
    pub id: i64,
    pub position: i64,
    pub content: EntryContent,
}

impl PlaylistEntry {
    pub fn track_id(&self) -> Option<i64> {
        match &self.content {
            EntryContent::Local(id) => Some(*id),
            EntryContent::Service(_) => None,
        }
    }

    pub fn service(&self) -> Option<&ServiceEntry> {
        match &self.content {
            EntryContent::Local(_) => None,
            EntryContent::Service(s) => Some(s),
        }
    }
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
    /// [`get`](Self::get) is kept for the internal paths that legitimately have no caller
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

    /// Remplace `ancienne` par `nouvelle` dans la playlist, à la même
    /// position — le geste « Remplacer » de la récupération de playlist
    /// (#3685). Rend le nombre de lignes RÉELLEMENT touchées : `0` quand
    /// `ancienne` n'y figurait pas, ou quand `nouvelle` n'existe pas dans
    /// `tracks` (voir `sql::replace_track`). L'appelant, qui a déjà tranché ces
    /// deux cas pour nommer son motif, ne doit donc jamais lire `0` comme un
    /// succès.
    ///
    /// Si `nouvelle` figure DÉJÀ dans la playlist, la ligne d'`ancienne` est
    /// retirée au lieu d'être réécrite : c'est l'invariant d'`add_tracks_deduped`
    /// — une playlist ne porte jamais deux fois la même piste — et un
    /// remplacement ne doit pas être le chemin par lequel il se perd. Le
    /// nombre rendu compte alors les lignes retirées.
    ///
    /// Une seule transaction : la lecture « est-elle déjà là ? » et l'écriture
    /// qu'elle décide ne peuvent pas être séparées par une autre écriture.
    pub fn replace_track(
        &self,
        playlist_id: i64,
        ancienne: i64,
        nouvelle: i64,
    ) -> Result<usize, String> {
        if ancienne == nouvelle {
            return Ok(0);
        }
        let contains_sql = self.dialect_sql(sql::contains_track, sql::contains_track);
        let replace_sql = self.dialect_sql(sql::replace_track, sql::replace_track);
        let delete_sql = self.dialect_sql(sql::delete_track_by_id, sql::delete_track_by_id);
        let mut touchees = 0usize;
        let touchees_ref = &mut touchees;
        self.db.write_tx(&mut |tx| {
            let cp: [&dyn ToSqlValue; 2] = [&playlist_id, &nouvelle];
            let deja_la = tx.query_one(&contains_sql, &cp)?.is_some();
            *touchees_ref = if deja_la {
                let dp: [&dyn ToSqlValue; 2] = [&playlist_id, &ancienne];
                tx.execute(&delete_sql, &dp)?
            } else {
                let rp: [&dyn ToSqlValue; 4] = [&nouvelle, &playlist_id, &ancienne, &nouvelle];
                tx.execute(&replace_sql, &rp)?
            };
            Ok(())
        })?;
        Ok(touchees)
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

    /// L'ancienne forme du réordonnancement : la liste COMPLÈTE des
    /// `tracks.id`, dans le nouvel ordre.
    ///
    /// Elle ne sait pas nommer un titre de service (#4889). Sur une playlist
    /// qui en porte, la réécrire telle quelle les EFFACERAIT : un client
    /// d'avant #4889 (ou l'écran v2, qui filtre `id` numérique) déplaçant une
    /// piste locale viderait la playlist de ses titres de service. Ces lignes
    /// gardent donc leur RANG ; les pistes locales reprennent, dans l'ordre
    /// demandé, les rangs qu'occupaient les pistes locales. Un identifiant en
    /// trop s'ajoute à la fin, un rang local sans identifiant disparaît —
    /// exactement ce que faisait l'ancienne réécriture pour une playlist
    /// sans titre de service, qui, elle, garde le chemin d'origine à
    /// l'identique. Le réordonnancement complet est
    /// [`reorder_by_ranks`](Self::reorder_by_ranks).
    pub fn reorder_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<(), String> {
        let lignes = self.get_entries(playlist_id)?;
        if lignes.iter().any(|l| l.service().is_some()) {
            let mut locales = track_ids.iter().copied();
            let mut suite: Vec<EntryContent> = Vec::with_capacity(lignes.len());
            for l in &lignes {
                match &l.content {
                    EntryContent::Service(_) => suite.push(l.content.clone()),
                    EntryContent::Local(_) => {
                        if let Some(tid) = locales.next() {
                            suite.push(EntryContent::Local(tid));
                        }
                    }
                }
            }
            suite.extend(locales.map(EntryContent::Local));
            return self.rewrite_entries(playlist_id, &suite);
        }
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

    /// Remplace tout le contenu par `suite`, dans l'ordre, en une transaction.
    fn rewrite_entries(&self, playlist_id: i64, suite: &[EntryContent]) -> Result<(), String> {
        let delete_sql = self.dialect_sql(sql::delete_all_tracks, sql::delete_all_tracks);
        let local_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        let service_sql = self.dialect_sql(sql::insert_service_entry, sql::insert_service_entry);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&playlist_id];
            tx.execute(&delete_sql, &p)?;
            for (i, e) in suite.iter().enumerate() {
                Self::insert_entry(tx, &local_sql, &service_sql, playlist_id, i as i64, e)?;
            }
            Ok(())
        })
    }

    /// Toutes les lignes de la playlist, locales ET de service (#4889), dans
    /// l'ordre d'affichage. Le rang d'une ligne dans ce vecteur est l'indice
    /// que le client montre.
    pub fn get_entries(&self, playlist_id: i64) -> Result<Vec<PlaylistEntry>, String> {
        let sql = self.dialect_sql(sql::get_entries, sql::get_entries);
        let params: [&dyn ToSqlValue; 1] = [&playlist_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().filter_map(row_to_entry).collect())
    }

    /// La playlist porte-t-elle au moins un titre de service ?
    pub fn has_service_entries(&self, playlist_id: i64) -> Result<bool, String> {
        Ok(self
            .get_entries(playlist_id)?
            .iter()
            .any(|e| e.service().is_some()))
    }

    /// Écrit UNE ligne dans la transaction en cours.
    fn insert_entry(
        tx: &dyn super::backend::DbTxHandle,
        local_sql: &str,
        service_sql: &str,
        playlist_id: i64,
        pos: i64,
        contenu: &EntryContent,
    ) -> Result<(), String> {
        match contenu {
            EntryContent::Local(tid) => {
                let p: [&dyn ToSqlValue; 3] = [&playlist_id, tid, &pos];
                tx.execute(local_sql, &p)?;
            }
            EntryContent::Service(s) => {
                let p: [&dyn ToSqlValue; 10] = [
                    &playlist_id,
                    &pos,
                    &s.source,
                    &s.source_id,
                    &s.title,
                    &s.artist,
                    &s.album,
                    &s.album_source_id,
                    &s.duration_ms,
                    &s.cover_url,
                ];
                tx.execute(service_sql, &p)?;
            }
        }
        Ok(())
    }

    /// Le geste « Ajouter à une playlist » pour les DEUX sortes de lignes
    /// (#4889) : une piste locale ou un titre de service.
    ///
    /// Même contrat que [`add_tracks_deduped`](Self::add_tracks_deduped) : ce
    /// qui est déjà dans la playlist, ou répété dans le lot, n'entre pas une
    /// seconde fois. Un titre de service se reconnaît à sa paire
    /// `(source, source_id)` — le même titre Qobuz ajouté deux fois reste une
    /// seule ligne, comme une piste locale. Une seule transaction ; rend ce
    /// qui a RÉELLEMENT été écrit.
    pub fn add_entries_deduped(
        &self,
        playlist_id: i64,
        entries: &[EntryContent],
        position: Option<i64>,
    ) -> Result<Vec<EntryContent>, String> {
        let mut vues: std::collections::HashSet<_> = self
            .get_entries(playlist_id)?
            .iter()
            .map(|e| e.content.cle())
            .collect();
        let a_ecrire: Vec<EntryContent> = entries
            .iter()
            .filter(|e| vues.insert(e.cle()))
            .cloned()
            .collect();
        if a_ecrire.is_empty() {
            return Ok(Vec::new());
        }
        let max_pos_sql = self.dialect_sql(sql::max_position, sql::max_position);
        let local_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        let service_sql = self.dialect_sql(sql::insert_service_entry, sql::insert_service_entry);
        let a_ecrire_ref = &a_ecrire;
        self.db.write_tx(&mut |tx| {
            let mp: [&dyn ToSqlValue; 1] = [&playlist_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &mp)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let depart = position.unwrap_or(max_pos + 1);
            for (i, e) in a_ecrire_ref.iter().enumerate() {
                Self::insert_entry(
                    tx,
                    &local_sql,
                    &service_sql,
                    playlist_id,
                    depart + i as i64,
                    e,
                )?;
            }
            Ok(())
        })?;
        Ok(a_ecrire)
    }

    /// [`create_with_tracks`](Self::create_with_tracks) pour les deux sortes
    /// de lignes (#4889) — la « Dupliquer » d'une playlist qui porte des
    /// titres de service les recopie aussi. Tout ou rien, répétitions du lot
    /// écartées (première occurrence gardée).
    pub fn create_with_entries(
        &self,
        name: &str,
        description: Option<&str>,
        profile_id: i64,
        entries: &[EntryContent],
    ) -> Result<(i64, Vec<EntryContent>), String> {
        let create_sql = self.dialect_sql(sql::create, sql::create);
        let local_sql = self.dialect_sql(sql::insert_playlist_track, sql::insert_playlist_track);
        let service_sql = self.dialect_sql(sql::insert_service_entry, sql::insert_service_entry);
        let mut vues = std::collections::HashSet::new();
        let a_ecrire: Vec<EntryContent> = entries
            .iter()
            .filter(|e| vues.insert(e.cle()))
            .cloned()
            .collect();
        let mut new_id = 0i64;
        {
            let new_id_ref = &mut new_id;
            let a_ecrire_ref = &a_ecrire;
            self.db.write_tx(&mut |tx| {
                let cp: [&dyn ToSqlValue; 3] = [&name, &description, &profile_id];
                tx.execute(&create_sql, &cp)?;
                let id = tx.last_insert_rowid();
                *new_id_ref = id;
                for (i, e) in a_ecrire_ref.iter().enumerate() {
                    Self::insert_entry(tx, &local_sql, &service_sql, id, i as i64, e)?;
                }
                Ok(())
            })?;
        }
        Ok((new_id, a_ecrire))
    }

    /// Réordonne TOUTES les lignes, locales et de service (#4889).
    ///
    /// `rangs` est le nouvel ordre exprimé en RANGS actuels — les indices de
    /// [`get_entries`](Self::get_entries), c'est-à-dire ceux que le client
    /// affiche : `[2, 0, 1]` met la troisième ligne en tête. Il doit être une
    /// permutation exacte de `0..n` ; sinon rien n'est écrit et la fonction
    /// rend `false` (une liste périmée — une ligne ajoutée entre la lecture
    /// du client et sa demande — ne doit pas perdre ni dupliquer de ligne).
    ///
    /// Les lignes gardent leur `id` : seules les positions sont réécrites,
    /// et elles redeviennent contiguës.
    pub fn reorder_by_ranks(&self, playlist_id: i64, rangs: &[i64]) -> Result<bool, String> {
        let lignes = self.get_entries(playlist_id)?;
        let n = lignes.len();
        if rangs.len() != n {
            return Ok(false);
        }
        let mut vus = vec![false; n];
        for &r in rangs {
            match usize::try_from(r) {
                Ok(r) if r < n && !vus[r] => vus[r] = true,
                _ => return Ok(false),
            }
        }
        let set_sql = self.dialect_sql(sql::set_entry_position, sql::set_entry_position);
        let lignes_ref = &lignes;
        self.db.write_tx(&mut |tx| {
            for (nouvelle, &rang) in rangs.iter().enumerate() {
                let pos = nouvelle as i64;
                let p: [&dyn ToSqlValue; 3] = [&pos, &lignes_ref[rang as usize].id, &playlist_id];
                tx.execute(&set_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(true)
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

/// Une ligne de `get_entries`. `None` pour une ligne qui ne serait ni
/// locale ni de service — le CHECK l'interdit, mais une base passée par un
/// autre chemin ne doit pas faire tomber toute la liste pour une ligne.
fn row_to_entry(cols: &Vec<SqlValue>) -> Option<PlaylistEntry> {
    let id = cols.first().and_then(|v| v.as_i64())?;
    let position = cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let texte = |i: usize| cols.get(i).and_then(|v| v.as_string());
    let content = match cols.get(2).and_then(|v| v.as_i64()) {
        Some(tid) => EntryContent::Local(tid),
        None => EntryContent::Service(ServiceEntry {
            source: texte(3)?,
            source_id: texte(4)?,
            title: texte(5).unwrap_or_default(),
            artist: texte(6),
            album: texte(7),
            album_source_id: texte(8),
            duration_ms: cols.get(9).and_then(|v| v.as_i64()),
            cover_url: texte(10),
        }),
    };
    Some(PlaylistEntry {
        id,
        position,
        content,
    })
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

    fn trois_pistes(track_repo: &crate::db::track_repo::TrackRepo) -> Vec<i64> {
        ["/r1.flac", "/r2.flac", "/r3.flac"]
            .iter()
            .map(|p| {
                let mut t = TrackModel::new((*p).into());
                t.file_path = Some((*p).into());
                track_repo.create(&t).unwrap()
            })
            .collect()
    }

    /// #3685 — « Remplacer » réécrit la ligne À LA MÊME POSITION.
    #[test]
    fn replace_track_reecrit_la_piste_a_la_meme_position() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);

        let plid = repo.create("À récupérer", None, 1).unwrap();
        repo.add_tracks(plid, &[ids[0], ids[1]], None).unwrap();

        let touchees = repo.replace_track(plid, ids[0], ids[2]).unwrap();
        assert_eq!(touchees, 1, "une ligne réécrite");
        assert_eq!(
            repo.get_track_ids(plid).unwrap(),
            vec![ids[2], ids[1]],
            "la piste de remplacement prend la place de la piste manquante, en tête"
        );
    }

    /// Une piste absente de la playlist, ou une piste de remplacement qui
    /// n'existe pas dans `tracks`, ne touchent AUCUNE ligne : c'est ce `0` que
    /// la route lit pour ne pas annoncer un remplacement qui n'a pas eu lieu.
    #[test]
    fn replace_track_ne_touche_rien_hors_playlist_ni_vers_une_piste_inconnue() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);

        let plid = repo.create("À récupérer", None, 1).unwrap();
        repo.add_tracks(plid, &[ids[0]], None).unwrap();

        // `ids[1]` n'est pas dans la playlist.
        assert_eq!(repo.replace_track(plid, ids[1], ids[2]).unwrap(), 0);
        // `999_999` n'existe pas dans `tracks` : la garde EXISTS rend 0 sur
        // les deux moteurs, au lieu d'une clef étrangère qui ne parle que sur
        // SQLite.
        assert_eq!(repo.replace_track(plid, ids[0], 999_999).unwrap(), 0);
        assert_eq!(
            repo.get_track_ids(plid).unwrap(),
            vec![ids[0]],
            "rien ne doit avoir bougé"
        );
    }

    /// Si la piste de remplacement est DÉJÀ dans la playlist, la ligne
    /// manquante est retirée plutôt que réécrite en doublon (invariant
    /// d'`add_tracks_deduped`).
    #[test]
    fn replace_track_vers_une_piste_deja_presente_retire_le_doublon() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);

        let plid = repo.create("À récupérer", None, 1).unwrap();
        repo.add_tracks(plid, &[ids[0], ids[1], ids[2]], None)
            .unwrap();

        let touchees = repo.replace_track(plid, ids[0], ids[2]).unwrap();
        assert_eq!(touchees, 1, "la ligne de la piste manquante est retirée");
        assert_eq!(
            repo.get_track_ids(plid).unwrap(),
            vec![ids[1], ids[2]],
            "pas de doublon de la piste de remplacement"
        );
    }

    // --- #4889 : titres de service dans une playlist Tune ---------------

    fn titre(source: &str, id: &str, nom: &str) -> ServiceEntry {
        ServiceEntry {
            source: source.into(),
            source_id: id.into(),
            title: nom.into(),
            artist: Some("Artiste".into()),
            album: Some("Album".into()),
            album_source_id: Some("alb-1".into()),
            duration_ms: Some(431_000),
            cover_url: Some("https://exemple/pochette.jpg".into()),
        }
    }

    fn sortes(repo: &PlaylistRepo, plid: i64) -> Vec<String> {
        repo.get_entries(plid)
            .unwrap()
            .iter()
            .map(|e| match &e.content {
                EntryContent::Local(id) => format!("L{id}"),
                EntryContent::Service(s) => format!("{}:{}", s.source, s.source_id),
            })
            .collect()
    }

    /// Un titre de service ENTRE dans une playlist Tune, à la suite des pistes
    /// locales, et se relit tel qu'il a été écrit.
    #[test]
    fn un_titre_de_service_entre_et_se_relit() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Mixte", None, 1).unwrap();
        repo.add_tracks(plid, &[ids[0]], None).unwrap();

        let q = titre("qobuz", "52818331", "Sinfonia");
        let ecrites = repo
            .add_entries_deduped(
                plid,
                &[
                    EntryContent::Service(q.clone()),
                    EntryContent::Local(ids[1]),
                ],
                None,
            )
            .unwrap();
        assert_eq!(ecrites.len(), 2);
        assert_eq!(
            sortes(&repo, plid),
            vec![
                format!("L{}", ids[0]),
                "qobuz:52818331".to_string(),
                format!("L{}", ids[1])
            ]
        );
        let lu = repo.get_entries(plid).unwrap()[1].clone();
        assert_eq!(lu.service(), Some(&q), "le titre se relit à l'identique");
        assert_eq!(repo.get(plid).unwrap().unwrap().track_count, 3);
        // Les appelants qui raisonnent en `tracks.id` ne voient que les
        // pistes locales, sans trou ni NUL.
        assert_eq!(repo.get_track_ids(plid).unwrap(), vec![ids[0], ids[1]]);
    }

    /// Le même titre de service ajouté deux fois reste UNE ligne — comme une
    /// piste locale. Deux services différents, même identifiant : deux titres.
    #[test]
    fn le_meme_titre_de_service_n_entre_qu_une_fois() {
        let db = test_db();
        let repo = PlaylistRepo::new(db);
        let plid = repo.create("Mixte", None, 1).unwrap();
        let q = EntryContent::Service(titre("qobuz", "1", "Un"));
        let t = EntryContent::Service(titre("tidal", "1", "Un"));
        let premier = repo
            .add_entries_deduped(plid, &[q.clone(), q.clone(), t.clone()], None)
            .unwrap();
        assert_eq!(premier, vec![q.clone(), t], "répétition du lot écartée");
        let second = repo.add_entries_deduped(plid, &[q], None).unwrap();
        assert!(second.is_empty(), "déjà dans la playlist : rien n'entre");
        assert_eq!(sortes(&repo, plid), vec!["qobuz:1", "tidal:1"]);
    }

    /// L'ancienne forme du réordonnancement (`track_ids` seuls) ne sait pas
    /// nommer un titre de service : elle ne doit PAS l'effacer. Les titres de
    /// service gardent leur rang, les pistes locales prennent l'ordre demandé.
    #[test]
    fn l_ancien_reordonnancement_garde_les_titres_de_service() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Mixte", None, 1).unwrap();
        repo.add_entries_deduped(
            plid,
            &[
                EntryContent::Local(ids[0]),
                EntryContent::Service(titre("bandcamp", "b-7", "Bandcamp")),
                EntryContent::Local(ids[1]),
            ],
            None,
        )
        .unwrap();

        repo.reorder_tracks(plid, &[ids[1], ids[0]]).unwrap();
        assert_eq!(
            sortes(&repo, plid),
            vec![
                format!("L{}", ids[1]),
                "bandcamp:b-7".to_string(),
                format!("L{}", ids[0])
            ],
            "le titre Bandcamp reste au milieu, les locales sont permutées"
        );
        let s = repo.get_entries(plid).unwrap()[1]
            .service()
            .cloned()
            .unwrap();
        assert_eq!(s.title, "Bandcamp", "ses colonnes d'affichage survivent");
    }

    /// Contre-épreuve du test ci-dessus : la réécriture d'AVANT #4889
    /// (tout effacer, réinsérer les `track_ids`) perd bien le titre de
    /// service. C'est ce que le chemin mixte évite.
    #[test]
    fn contre_epreuve_la_reecriture_nue_perdait_le_titre_de_service() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Mixte", None, 1).unwrap();
        repo.add_entries_deduped(
            plid,
            &[
                EntryContent::Local(ids[0]),
                EntryContent::Service(titre("bandcamp", "b-7", "Bandcamp")),
            ],
            None,
        )
        .unwrap();
        repo.set_tracks(plid, &[ids[0]]).unwrap();
        assert_eq!(sortes(&repo, plid), vec![format!("L{}", ids[0])]);
    }

    /// Sur une playlist toute locale, l'ancien réordonnancement ne change pas
    /// d'un iota (non-régression).
    #[test]
    fn l_ancien_reordonnancement_d_une_playlist_locale_est_inchange() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Locale", None, 1).unwrap();
        repo.add_tracks(plid, &[ids[0], ids[1]], None).unwrap();
        // Une liste plus longue que la playlist la réécrit entière, comme avant.
        repo.reorder_tracks(plid, &[ids[2], ids[1], ids[0]])
            .unwrap();
        assert_eq!(
            repo.get_track_ids(plid).unwrap(),
            vec![ids[2], ids[1], ids[0]]
        );
    }

    /// Le réordonnancement par RANGS déplace aussi les titres de service, et
    /// refuse — sans rien écrire — ce qui n'est pas une permutation exacte.
    #[test]
    fn reordonner_par_rangs_deplace_tout_et_refuse_une_liste_perimee() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Mixte", None, 1).unwrap();
        repo.add_entries_deduped(
            plid,
            &[
                EntryContent::Local(ids[0]),
                EntryContent::Service(titre("qobuz", "9", "Neuf")),
                EntryContent::Local(ids[1]),
            ],
            None,
        )
        .unwrap();

        assert!(repo.reorder_by_ranks(plid, &[1, 2, 0]).unwrap());
        assert_eq!(
            sortes(&repo, plid),
            vec![
                "qobuz:9".to_string(),
                format!("L{}", ids[1]),
                format!("L{}", ids[0])
            ]
        );
        let avant = sortes(&repo, plid);
        let perimees: [&[i64]; 5] = [&[0, 1], &[0, 1, 1], &[0, 1, 3], &[0, 1, 2, 3], &[-1, 0, 1]];
        for perimee in perimees {
            assert!(
                !repo.reorder_by_ranks(plid, perimee).unwrap(),
                "{perimee:?} n'est pas une permutation"
            );
            assert_eq!(
                sortes(&repo, plid),
                avant,
                "rien n'a bougé pour {perimee:?}"
            );
        }
    }

    /// « Dupliquer » recopie aussi les titres de service.
    #[test]
    fn dupliquer_recopie_les_titres_de_service() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Mixte", None, 1).unwrap();
        repo.add_entries_deduped(
            plid,
            &[
                EntryContent::Service(titre("youtube", "yt-1", "Vidéo")),
                EntryContent::Local(ids[2]),
            ],
            None,
        )
        .unwrap();
        let contenu: Vec<EntryContent> = repo
            .get_entries(plid)
            .unwrap()
            .into_iter()
            .map(|e| e.content)
            .collect();
        let (copie, ecrites) = repo
            .create_with_entries("Mixte (copy)", None, 1, &contenu)
            .unwrap();
        assert_eq!(ecrites, contenu);
        assert_eq!(sortes(&repo, copie), sortes(&repo, plid));
    }

    /// Doublon de la même piste LOCALE : l'ajout brut le permet comme avant
    /// (fusion sans dédoublonnage, playlists de dossier) — aucune clé
    /// d'unicité n'a été posée.
    #[test]
    fn doublon_local_brut_toujours_permis() {
        let db = test_db();
        let track_repo = crate::db::track_repo::TrackRepo::new(db.clone());
        let repo = PlaylistRepo::new(db);
        let ids = trois_pistes(&track_repo);
        let plid = repo.create("Doublons", None, 1).unwrap();
        repo.add_tracks(plid, &[ids[0], ids[0]], None).unwrap();
        assert_eq!(repo.get_track_ids(plid).unwrap(), vec![ids[0], ids[0]]);
    }

    /// Le client envoie un `StreamingTrackInfo` ; il manque un de ces trois
    /// champs — ou la source est `local` — et le titre n'est pas un titre de
    /// service enregistrable.
    #[test]
    fn un_titre_de_service_se_lit_depuis_le_corps_du_client() {
        let complet = serde_json::json!({
            "source": "bandcamp", "source_id": 123, "title": "Nuit",
            "artist_name": "Moi", "album_title": "Disque", "album_id": "a-9",
            "duration_ms": 200000, "cover_path": "https://c/p.jpg"
        });
        let lu = ServiceEntry::from_json(&complet).unwrap();
        assert_eq!(
            lu.source_id, "123",
            "un identifiant numérique devient texte"
        );
        assert_eq!(lu.album_source_id.as_deref(), Some("a-9"));
        assert_eq!(lu.duration_ms, Some(200_000));
        let rendu = lu.to_json();
        assert_eq!(rendu["id"], serde_json::Value::Null);
        assert_eq!(rendu["source"], "bandcamp");
        assert_eq!(rendu["artist_name"], "Moi");
        assert_eq!(rendu["cover_path"], "https://c/p.jpg");

        for manque in ["source", "source_id", "title"] {
            let mut v = complet.clone();
            v.as_object_mut().unwrap().remove(manque);
            assert!(ServiceEntry::from_json(&v).is_none(), "sans {manque}");
        }
        let mut local = complet.clone();
        local["source"] = "local".into();
        assert!(
            ServiceEntry::from_json(&local).is_none(),
            "`local` n'est pas un service"
        );
    }

    /// #4889 sur PostgreSQL, par les DEUX naissances d'une base : installation
    /// native (scripts numérotés 001 → 072) et bascule depuis SQLite
    /// (`PG_FULL_SCHEMA` tout-TEXT, une ligne copiée, puis les scripts
    /// rejoués). SQLite tolère ce que PostgreSQL refuse : c'est ici que se
    /// voient un `text = bigint` ou un NOT NULL resté en place.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_4889_titres_de_service_sur_les_deux_naissances() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };

        async fn base(url: &str, schema: &'static str) -> sqlx::PgPool {
            let maintenance = sqlx::PgPool::connect(url).await.unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
            )))
            .execute(&maintenance)
            .await
            .unwrap();
            maintenance.close().await;
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .after_connect(move |c, _| {
                    Box::pin(async move {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
                            .execute(c)
                            .await
                            .map(|_| ())
                    })
                })
                .connect(url)
                .await
                .unwrap()
        }

        async fn duree_et_contrainte(pool: &sqlx::PgPool) -> (String, String, i64) {
            let (t, nul): (String, String) = sqlx::query_as(
                "SELECT (SELECT data_type::text FROM information_schema.columns \
                          WHERE table_schema = current_schema() \
                            AND table_name = 'playlist_tracks' AND column_name = 'duration_ms'), \
                        (SELECT is_nullable::text FROM information_schema.columns \
                          WHERE table_schema = current_schema() \
                            AND table_name = 'playlist_tracks' AND column_name = 'track_id')",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_constraint \
                 WHERE conname = 'playlist_tracks_piste_ou_titre_de_service' \
                   AND conrelid = 'playlist_tracks'::regclass",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            (t, nul, n)
        }

        fn scenario(repo: &PlaylistRepo, locale: i64, deja: Option<i64>) {
            let plid = deja.unwrap_or_else(|| repo.create("Mixte PG", None, 1).unwrap());
            let q = ServiceEntry {
                source: "qobuz".into(),
                source_id: "52818331".into(),
                title: "Sinfonia".into(),
                artist: Some("Sammartini".into()),
                album: None,
                album_source_id: Some("alb".into()),
                duration_ms: Some(431_000),
                cover_url: None,
            };
            repo.add_entries_deduped(
                plid,
                &[
                    EntryContent::Service(q.clone()),
                    EntryContent::Local(locale),
                    EntryContent::Service(q.clone()),
                ],
                None,
            )
            .unwrap();
            let lignes = repo.get_entries(plid).unwrap();
            let services: Vec<&ServiceEntry> = lignes.iter().filter_map(|l| l.service()).collect();
            assert_eq!(
                services,
                vec![&q],
                "un seul titre de service, relu à l'identique"
            );
            assert!(repo.get_track_ids(plid).unwrap().contains(&locale));
            let n = lignes.len() as i64;
            let inverse: Vec<i64> = (0..n).rev().collect();
            assert!(repo.reorder_by_ranks(plid, &inverse).unwrap());
            let contenu: Vec<EntryContent> = repo
                .get_entries(plid)
                .unwrap()
                .into_iter()
                .map(|e| e.content)
                .collect();
            let (copie, _) = repo
                .create_with_entries("Copie PG", None, 1, &contenu)
                .unwrap();
            assert_eq!(
                repo.get_entries(copie)
                    .unwrap()
                    .into_iter()
                    .map(|e| e.content)
                    .collect::<Vec<_>>(),
                contenu
            );
            // L'ancienne forme garde le titre de service.
            let locales = repo.get_track_ids(plid).unwrap();
            repo.reorder_tracks(plid, &locales).unwrap();
            assert!(
                repo.get_entries(plid)
                    .unwrap()
                    .iter()
                    .any(|e| e.service() == Some(&q))
            );
        }

        // 1. Installation native : la table telle que 001 la crée
        //    (`track_id BIGINT NOT NULL REFERENCES tracks(id)`), puis la 072.
        //    PAS `run_pg_migrations` dans un schéma de côté : la CI partage UNE
        //    base, dont le schéma `public` est déjà migré, et la 012 relève les
        //    déclencheurs dans `pg_trigger` SANS filtrer le schéma — elle
        //    trouvait ceux de `public` et échouait à les retirer ici
        //    (« trigger upnp_revision_tracks_insert does not exist »). Même
        //    découpe que `pg_4853` : on monte ce que la 072 doit trouver.
        let pool = base(&url, "playlists_4889_natif").await;
        sqlx::raw_sql(include_str!(
            "../../migrations/postgres/001_initial_schema.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        // `profile_id` : posée par `ensure_schema` (postgres.rs) sur toute
        // base, et lue par `PlaylistRepo::create`.
        sqlx::raw_sql(
            "ALTER TABLE playlists ADD COLUMN IF NOT EXISTS profile_id BIGINT NOT NULL DEFAULT 1",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/postgres/072_playlist_tracks_titres_de_service.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            duree_et_contrainte(&pool).await,
            ("bigint".to_string(), "YES".to_string(), 1)
        );
        let backend: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        let locale: i64 = sqlx::query_scalar(
            "INSERT INTO tracks (title, source) VALUES ('Locale', 'local') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        scenario(&PlaylistRepo::with_backend(backend), locale, None);
        pool.close().await;

        // 2. Bascule : tout-TEXT, une playlist copiée depuis SQLite (une ligne
        //    locale, une ligne de service), puis la 072 — deux fois. Même
        //    découpe que `pg_4853` : les autres rattrapages de types (010-013)
        //    ont leurs propres bancs, celui-ci garde ce que la 072 doit faire
        //    sur ce chemin-là.
        let pool = base(&url, "playlists_4889_bascule").await;
        sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS schema_version (
                 version INTEGER PRIMARY KEY, name TEXT, applied_at TIMESTAMPTZ DEFAULT now());
             INSERT INTO playlist_tracks (id, playlist_id, track_id, position) VALUES ('8', '3', '41', '0');
             INSERT INTO playlist_tracks (id, playlist_id, track_id, position, source, source_id, title, duration_ms)
                 VALUES ('9', '3', NULL, '1', 'bandcamp', 'bc-1', 'Copié de SQLite', '1000');",
        )
        .execute(&pool)
        .await
        .unwrap();
        const MIGRATION: &str =
            include_str!("../../migrations/postgres/072_playlist_tracks_titres_de_service.sql");
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
        // Rejouée : strict no-op.
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
        assert_eq!(
            duree_et_contrainte(&pool).await,
            ("bigint".to_string(), "YES".to_string(), 1)
        );
        let copiee: (Option<i64>, String) =
            sqlx::query_as("SELECT duration_ms, source FROM playlist_tracks WHERE id = '9'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(copiee, (Some(1000), "bandcamp".to_string()));
        assert!(
            sqlx::raw_sql(
                "INSERT INTO playlist_tracks (id, playlist_id, track_id, position, source, source_id) \
                 VALUES ('10', '3', '41', '2', 'qobuz', '1')"
            )
            .execute(&pool)
            .await
            .is_err(),
            "le CHECK refuse une ligne locale ET de service"
        );
        pool.close().await;

        // Nettoyage OBLIGATOIRE : la CI partage UNE base entre ses étapes (voir
        // `pg_4853_dossiers_sur_les_deux_naissances`).
        let maintenance = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql(
            "DROP SCHEMA IF EXISTS playlists_4889_natif CASCADE; \
             DROP SCHEMA IF EXISTS playlists_4889_bascule CASCADE;",
        )
        .execute(&maintenance)
        .await
        .unwrap();
        maintenance.close().await;
    }
}
