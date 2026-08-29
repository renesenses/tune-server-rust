use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::models::Album;
use super::sqlite::SqliteDb;
use crate::TuneError;

/// Engine-agnostic SQL builders for album_repo.
pub mod sql {
    use super::SqlDialect;

    /// ⚠️ L'ordre des colonnes EST le contrat de [`super::row_to_album`], qui
    /// lit par index. `is_compilation` est en 24 ; une éventuelle colonne 25
    /// (`added_at`) reste tolérée par `row_to_album` : toute colonne ajoutée
    /// ici doit l'être AVANT `FROM`, et `row_to_album` mis à jour dans le
    /// même mouvement.
    pub fn select_album() -> &'static str {
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.original_year, a.genre, a.disc_count, a.track_count, a.cover_path, a.source, a.source_id, a.label, a.catalog_number, a.barcode, a.format, a.sample_rate, a.bit_depth, a.bio, a.musicbrainz_release_id, a.musicbrainz_release_group_id, a.release_date, a.original_date, a.genres, a.is_compilation FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id"
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("{} WHERE a.id = {}", select_album(), d.placeholder(1))
    }

    pub fn get_by_title<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE LOWER(a.title) = LOWER({}) LIMIT 1",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn get_by_title_only<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.title = {} LIMIT 1",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn get_by_title_artist_year<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE LOWER(a.title) = LOWER({}) AND a.artist_id = {} AND a.year = {}",
            select_album(),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn get_by_title_artist<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE LOWER(a.title) = LOWER({}) AND a.artist_id = {}",
            select_album(),
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_by_musicbrainz_release_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.musicbrainz_release_id = {}",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO albums (title, artist_id, year, original_year, genre, genres, disc_count, track_count, cover_path, source, source_id, label, catalog_number, barcode, format, sample_rate, bit_depth, bio, musicbrainz_release_id, musicbrainz_release_group_id, release_date, original_date, is_compilation) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11),
            d.placeholder(12),
            d.placeholder(13),
            d.placeholder(14),
            d.placeholder(15),
            d.placeholder(16),
            d.placeholder(17),
            d.placeholder(18),
            d.placeholder(19),
            d.placeholder(20),
            d.placeholder(21),
            d.placeholder(22),
            d.placeholder(23),
        )
    }

    pub fn create_minimal<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO albums (title, artist_id, year) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn create_with_mbid<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO albums (title, artist_id, year, musicbrainz_release_id) VALUES ({}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn get_id_by_folder<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id FROM albums WHERE folder_path = {} LIMIT 1",
            d.placeholder(1)
        )
    }

    /// Albums homonymes déjà rattachés à un AUTRE dossier, avec leur dossier
    /// et les numéros de piste qu'ils occupent : de quoi décider si le dossier
    /// courant est l'éclat d'une même compilation (#1440).
    pub fn scattered_candidates<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT a.id, a.folder_path, \
             (SELECT GROUP_CONCAT(t.track_number) FROM tracks t WHERE t.album_id = a.id) \
             FROM albums a \
             WHERE LOWER(a.title) = LOWER({}) AND a.folder_path IS NOT NULL \
             AND a.folder_path <> {} LIMIT 50",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_folder_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT folder_path FROM albums WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_folder_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET folder_path = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Lève le drapeau « compilation » (#1957). `COALESCE(is_compilation, 0)`
    /// et non `is_compilation = 0` : une base migrée peut porter des NULL, et
    /// `NULL = 0` est NULL — la ligne ne serait jamais mise à jour.
    pub fn mark_compilation<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET is_compilation = 1 \
             WHERE id = {} AND COALESCE(is_compilation, 0) = 0",
            d.placeholder(1)
        )
    }

    pub fn get_artist_name<D: SqlDialect>(d: &D) -> String {
        format!("SELECT name FROM artists WHERE id = {}", d.placeholder(1))
    }

    pub fn set_artist_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET artist_id = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Albums qui portent la signature étroite du collage #2458.
    ///
    /// On ne répare PAS tout désaccord album/pistes : un album classique peut
    /// légitimement porter un artiste d'album différent de ses interprètes. Il
    /// faut ici, simultanément :
    /// - un artiste d'album dont le MBID est présent mais vide (valeur invalide
    ///   que l'ancien `ArtistRepo::get_or_create` utilisait comme identité) ;
    /// - un album local non compilation ;
    /// - toutes les pistes rattachées au même autre `artist_id` ;
    /// - aucun tag ALBUMARTIST non vide qui contredise cet artiste unanime.
    pub fn empty_mbid_artist_collapse_candidates() -> &'static str {
        "SELECT a.id, a.title, a.artist_id, unanimous.artist_id, target.name \
         FROM albums a \
         JOIN artists current_artist ON current_artist.id = a.artist_id \
         JOIN ( \
             SELECT album_id, MIN(artist_id) AS artist_id \
             FROM tracks \
             GROUP BY album_id \
             HAVING COUNT(*) > 0 \
                AND COUNT(artist_id) = COUNT(*) \
                AND COUNT(DISTINCT artist_id) = 1 \
         ) unanimous ON unanimous.album_id = a.id \
         JOIN artists target ON target.id = unanimous.artist_id \
         WHERE a.source = 'local' \
           AND COALESCE(a.is_compilation, 0) = 0 \
           AND current_artist.musicbrainz_id IS NOT NULL \
           AND TRIM(current_artist.musicbrainz_id) = '' \
           AND a.artist_id <> unanimous.artist_id \
           AND NOT EXISTS ( \
               SELECT 1 FROM tracks tagged \
               WHERE tagged.album_id = a.id \
                 AND NULLIF(TRIM(tagged.album_artist), '') IS NOT NULL \
                 AND LOWER(TRIM(tagged.album_artist)) <> LOWER(TRIM(target.name)) \
           )"
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET title = {}, artist_id = {}, year = {}, original_year = {}, genre = {}, genres = {}, disc_count = {}, track_count = {}, cover_path = {}, label = {}, catalog_number = {}, format = {}, sample_rate = {}, bit_depth = {}, bio = {}, musicbrainz_release_id = {}, musicbrainz_release_group_id = {}, release_date = {}, original_date = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11),
            d.placeholder(12),
            d.placeholder(13),
            d.placeholder(14),
            d.placeholder(15),
            d.placeholder(16),
            d.placeholder(17),
            d.placeholder(18),
            d.placeholder(19),
            d.placeholder(20),
        )
    }

    /// Update album date fields using COALESCE so we only fill in
    /// values that are not already set.
    pub fn update_dates<D: SqlDialect>(d: &D) -> String {
        // `year` is COALESCE'd like the other date fields: it's set at album
        // creation from the first track that creates the row, but if THAT track's
        // year was missing (e.g. its tag read errored during the scan) the album
        // was left with year = NULL forever — no later track back-filled it, so
        // "sort/filter by year" and the "missing year" list were wrong even though
        // the files had the year (Bilou, #1106). COALESCE fills NULL from any
        // track that carries a year, and never overwrites an existing value.
        format!(
            "UPDATE albums SET year = COALESCE(year, {}), original_year = COALESCE(original_year, {}), release_date = COALESCE(release_date, {}), original_date = COALESCE(original_date, {}) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5)
        )
    }

    /// Ecrit l'annee EN ECRASANT la valeur existante.
    ///
    /// `update_dates` ci-dessus fait un COALESCE et ne remplace jamais rien :
    /// c'est ce qu'il faut quand on comble un trou laisse par le scan. Une
    /// correction validee par l'utilisateur est le cas oppose — la valeur en
    /// place est justement celle qu'il veut changer. Deux besoins contraires,
    /// deux requetes ; les confondre rendrait l'arbitrage sans effet, en
    /// silence.
    pub fn set_year<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET year = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_cover_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET cover_path = COALESCE(cover_path, {}) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn force_update_cover_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET cover_path = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_track_count<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE albums SET track_count = (SELECT COUNT(*) FROM tracks WHERE album_id = {}) WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM albums WHERE id = {}", d.placeholder(1))
    }

    pub fn count_orphans() -> &'static str {
        "SELECT COUNT(*) FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)"
    }

    pub fn delete_orphans() -> &'static str {
        "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks WHERE album_id IS NOT NULL)"
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM albums"
    }

    /// Compteur de la GRILLE : même exclusion des albums masqués que
    /// `list_filtered`, sinon le `total` de la pagination ment et la grille
    /// saute ou duplique des pages (#1391).
    pub fn count_visible() -> String {
        format!(
            "SELECT COUNT(*) FROM albums a WHERE {}",
            crate::db::facet_filter::hidden_albums_excluded()
        )
    }

    pub fn list_recent<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE {} ORDER BY a.id DESC LIMIT {}",
            select_album(),
            crate::db::facet_filter::hidden_albums_excluded(),
            d.placeholder(1)
        )
    }

    pub fn list_by_release_group<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.musicbrainz_release_group_id = {} ORDER BY a.year ASC, LOWER(a.title) ASC",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn list_release_groups() -> String {
        format!(
            "{} WHERE a.musicbrainz_release_group_id IS NOT NULL ORDER BY a.musicbrainz_release_group_id, a.year ASC",
            select_album()
        )
    }

    pub fn list_by_artist<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.artist_id = {} AND {} ORDER BY a.year ASC, LOWER(a.title) ASC",
            select_album(),
            d.placeholder(1),
            crate::db::facet_filter::hidden_albums_excluded()
        )
    }

    pub fn list_by_year<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE a.year = {} ORDER BY LOWER(a.title) ASC",
            select_album(),
            d.placeholder(1)
        )
    }

    pub fn list_without_cover() -> &'static str {
        "SELECT a.id, a.title, ar.name, a.musicbrainz_release_id FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE (a.cover_path IS NULL OR a.cover_path = '') AND a.source = 'local' ORDER BY a.id"
    }

    pub fn list_without_bio() -> &'static str {
        "SELECT a.id, a.title, ar.name FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE (a.bio IS NULL OR a.bio = '') AND a.source = 'local' ORDER BY a.id"
    }

    pub fn count_with_bio() -> &'static str {
        "SELECT COUNT(*) FROM albums WHERE bio IS NOT NULL AND bio != ''"
    }

    pub fn list_with_bio_and_mbid() -> &'static str {
        "SELECT a.id, a.title, ar.name, a.musicbrainz_release_group_id, a.bio FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE a.bio IS NOT NULL AND a.bio != '' AND a.musicbrainz_release_group_id IS NOT NULL AND a.musicbrainz_release_group_id != '' ORDER BY a.id"
    }

    pub fn list_with_bio() -> &'static str {
        "SELECT a.id, a.title, ar.name, a.musicbrainz_release_group_id, a.bio, a.bio_source, a.bio_source_url, a.bio_license, a.bio_lang FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE a.bio IS NOT NULL AND a.bio != '' ORDER BY a.id"
    }

    pub fn list_without_bio_with_mbid() -> &'static str {
        "SELECT a.id, a.musicbrainz_release_group_id FROM albums a WHERE (a.bio IS NULL OR a.bio = '') AND a.musicbrainz_release_group_id IS NOT NULL AND a.musicbrainz_release_group_id != '' ORDER BY a.id"
    }

    pub fn list_without_bio_without_mbid() -> &'static str {
        "SELECT a.id, a.title, ar.name FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id WHERE (a.bio IS NULL OR a.bio = '') AND (a.musicbrainz_release_group_id IS NULL OR a.musicbrainz_release_group_id = '') AND a.source = 'local' ORDER BY a.id"
    }

    /// Le OU des critères est PARENTHÉSÉ pour recevoir le filtre « pas
    /// masqué » en ET — et ce filtre s'applique APRÈS la passe FTS : les
    /// index `albums_fts` contiennent tout, les reconstruire à chaque
    /// masquage serait le mauvais échange (#1391).
    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE (({}) OR LOWER(unaccent(a.title)) LIKE LOWER(unaccent({})) OR LOWER(unaccent(ar.name)) LIKE LOWER(unaccent({})) OR LOWER(unaccent(a.genre)) LIKE LOWER(unaccent({})) OR a.musicbrainz_release_id = {} OR EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND LOWER(unaccent(t.title)) LIKE LOWER(unaccent({})))) AND {} LIMIT {}",
            select_album(),
            d.fts_where("albums", "a", &d.placeholder(1)),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            crate::db::facet_filter::hidden_albums_excluded(),
            d.placeholder(7)
        )
    }
}

/// Une tranche de Dynamic Range, bornes **incluses** (#2144).
///
/// # Pourquoi une tranche libre, et non une liste de tranches figées
///
/// L'issue ne fixe AUCUNE borne : MinimServer y est cité en modèle
/// (« Minimserver permet ce tri de DR par range de DR », Patatorz, 15/08) mais
/// ses bornes exactes n'ont jamais été relevées, et la couverture réelle des
/// bibliothèques en tags DR n'a jamais été mesurée. Graver ici un découpage
/// inventé le figerait dans le contrat HTTP, où il survivrait à la mesure qui
/// le contredirait. Le serveur rend donc une tranche quelconque — tout
/// découpage, y compris celui de MinimServer le jour où il sera relevé,
/// s'exprime en `[min, max]` — et [`AlbumRepo::dynamic_range_values`] dit
/// quelles valeurs existent vraiment pour que le client dessine ses pastilles
/// sur des données, pas sur une hypothèse.
///
/// Une tranche est toujours RESTRICTIVE : un album sans tag DR n'en fait
/// jamais partie, quelles que soient les bornes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrRange {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

impl DrRange {
    /// `None` quand aucune borne n'est donnée : **pas de filtre du tout**,
    /// jamais un filtre qui laisserait tout passer ni un filtre vide (piège
    /// n°1 de `facet_filter`). Une tranche à l'envers (`min > max`) est
    /// conservée telle quelle et ne rend rien : c'est ce que l'appelant a
    /// demandé, et le mentir en l'inversant cacherait un bug d'interface.
    pub fn new(min: Option<i64>, max: Option<i64>) -> Option<Self> {
        if min.is_none() && max.is_none() {
            return None;
        }
        Some(Self { min, max })
    }
}

pub struct AlbumRepo {
    db: Arc<dyn DbBackend>,
}

impl AlbumRepo {
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

    pub fn get(&self, id: i64) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    pub fn get_by_title(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title, sql::get_by_title);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    /// Like `get_by_title` but reads through the write connection.
    /// Used by the scanner when running inside a `BEGIN IMMEDIATE` to
    /// see albums created earlier in the same transaction.
    pub fn get_by_title_strong(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title, sql::get_by_title);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .as_ref()
            .map(row_to_album))
    }

    pub fn get_by_title_and_artist(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Option<Album>, TuneError> {
        if let Some(y) = year {
            let sql =
                self.dialect_sql(sql::get_by_title_artist_year, sql::get_by_title_artist_year);
            let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &y];
            if let Some(row) = self.db.query_one(&sql, &params)? {
                return Ok(Some(row_to_album(&row)));
            }
        }
        let sql = self.dialect_sql(sql::get_by_title_artist, sql::get_by_title_artist);
        let params: [&dyn ToSqlValue; 2] = [&title, &artist_id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    pub fn get_by_title_only(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title_only, sql::get_by_title_only);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    /// Like `get_by_title_only` but reads through the write connection.
    /// Used by the scanner when running inside a `BEGIN IMMEDIATE` to
    /// see albums created earlier in the same transaction.
    pub fn get_by_title_only_strong(&self, title: &str) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_title_only, sql::get_by_title_only);
        let params: [&dyn ToSqlValue; 1] = [&title];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .as_ref()
            .map(row_to_album))
    }

    pub fn get_by_musicbrainz_release_id(
        &self,
        release_id: &str,
    ) -> Result<Option<Album>, TuneError> {
        let sql = self.dialect_sql(
            sql::get_by_musicbrainz_release_id,
            sql::get_by_musicbrainz_release_id,
        );
        let params: [&dyn ToSqlValue; 1] = [&release_id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_album))
    }

    pub fn create(&self, album: &Album) -> Result<i64, TuneError> {
        let sql = self.dialect_sql(sql::create, sql::create);
        // `is_compilation` en 0/1 et non en booléen natif : SQLite n'a pas de
        // type booléen et la colonne PG est un SMALLINT 0/1 (convention du
        // schéma, cf. l'en-tête de 001_initial_schema.sql).
        let is_compilation: i64 = i64::from(album.is_compilation);
        let params: [&dyn ToSqlValue; 23] = [
            &album.title,
            &album.artist_id,
            &album.year,
            &album.original_year,
            &album.genre,
            &album.genres,
            &album.disc_count,
            &album.track_count,
            &album.cover_path,
            &album.source,
            &album.source_id,
            &album.label,
            &album.catalog_number,
            &album.barcode,
            &album.format,
            &album.sample_rate,
            &album.bit_depth,
            &album.bio,
            &album.musicbrainz_release_id,
            &album.musicbrainz_release_group_id,
            &album.release_date,
            &album.original_date,
            &is_compilation,
        ];
        Ok(self.db.execute_returning_id(&sql, &params)?)
    }

    /// Look up an album by (title, artist, year), or create it.
    /// Sequential `query_one_strong` + `execute` + `last_insert_rowid`
    /// (not `write_tx`) because the scanner holds `BEGIN IMMEDIATE`
    /// while calling this, and `write_tx` would try to start a nested
    /// `BEGIN DEFERRED` — same constraint as `zone_repo::create` (cf.
    /// commit `9f502c0`). On SQLite the write mutex serializes the
    /// three calls, so a concurrent `get_or_create` on another thread
    /// can't shift the rowid we read.
    ///
    /// Uses `query_one_strong` (write connection) instead of
    /// `query_one` (read pool) so that the SELECT sees albums created
    /// earlier in the same `BEGIN IMMEDIATE` transaction. Without this,
    /// the read-only connection's WAL snapshot does not include
    /// uncommitted writes, causing each track in a batch to create a
    /// separate album instead of reusing the one created by the first
    /// track.
    pub fn get_or_create(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Album, TuneError> {
        if let Some(found) = self.find_by_title_and_artist_strong(title, artist_id, year)? {
            return Ok(found);
        }
        let create_sql = self.dialect_sql(sql::create_minimal, sql::create_minimal);
        let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &year];
        let id = self.db.execute_returning_id(&create_sql, &params)?;
        let mut album = Album::new(title.to_string());
        album.id = Some(id);
        album.artist_id = Some(artist_id);
        album.year = year;
        Ok(album)
    }

    /// Like `get_or_create` but also checks MusicBrainz release ID
    /// first.
    ///
    /// Lookup cascade:
    /// 1. MusicBrainz release ID (exact match)
    /// 2. Title + artist_id (+ year if present) — case-insensitive title
    pub fn get_or_create_with_mbid(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
        mbid: Option<&str>,
    ) -> Result<Album, TuneError> {
        if let Some(release_id) = mbid {
            let sql = self.dialect_sql(
                sql::get_by_musicbrainz_release_id,
                sql::get_by_musicbrainz_release_id,
            );
            let params: [&dyn ToSqlValue; 1] = [&release_id];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                let mut album = row_to_album(&row);
                self.reclaim_unknown_artist(&mut album, artist_id)?;
                return Ok(album);
            }
        }
        if let Some(found) = self.find_by_title_and_artist_strong(title, artist_id, year)? {
            // Don't collapse two DISTINCT MusicBrainz releases that merely share
            // title+artist+year. If we were handed an MBID and the album matched
            // by title already carries a *different* one, they are separate
            // editions — fall through and create a new album instead of merging
            // (Dominique: two releases with distinct MUSICBRAINZ_ALBUMID were
            // being fused into one). When either side lacks an MBID we keep the
            // old behaviour so partially-tagged albums stay together.
            let conflicting_mbid = matches!(
                (mbid, found.musicbrainz_release_id.as_deref()),
                (Some(incoming), Some(existing)) if incoming != existing
            );
            if !conflicting_mbid {
                return Ok(found);
            }
        }
        let create_sql = self.dialect_sql(sql::create_with_mbid, sql::create_with_mbid);
        let params: [&dyn ToSqlValue; 4] = [&title, &artist_id, &year, &mbid];
        let id = self.db.execute_returning_id(&create_sql, &params)?;
        let mut album = Album::new(title.to_string());
        album.id = Some(id);
        album.artist_id = Some(artist_id);
        album.year = year;
        album.musicbrainz_release_id = mbid.map(String::from);
        Ok(album)
    }

    /// The album a folder holds, creating it if this is the folder's first track.
    ///
    /// The folder on disk is what identifies a release, so it is tried first.
    /// That is what keeps an edition together when its discs differ in sample
    /// rate — a box set mixing 24/192, 16/44.1 and 24/48 is one album, not three
    /// — and what keeps two separate rips of the same album apart without
    /// resorting to a "(96kHz/24bit)" suffix on the title.
    ///
    /// Falling back to [`Self::get_or_create_with_mbid`] keeps every existing
    /// rule intact (MusicBrainz release id, title + artist + year), with one
    /// added condition: a candidate already claimed by a *different* folder is
    /// not reused, or the second rip would be merged into the first. A candidate
    /// with no folder yet — every album indexed before this column existed —
    /// adopts this one, so a library converts as it is rescanned rather than
    /// duplicating.
    pub fn get_or_create_for_folder(
        &self,
        folder: &str,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
        mbid: Option<&str>,
    ) -> Result<Album, TuneError> {
        self.get_or_create_for_folder_with_track(folder, title, artist_id, year, mbid, None)
    }

    /// Variante qui connaît le numéro de la piste en cours d'indexation, seule
    /// information permettant de recoller une compilation éparpillée par
    /// artiste sans risquer de fusionner deux homonymes (#1440).
    pub fn get_or_create_for_folder_with_track(
        &self,
        folder: &str,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
        mbid: Option<&str>,
        track_number: Option<i32>,
    ) -> Result<Album, TuneError> {
        if folder.is_empty() {
            return self.get_or_create_with_mbid(title, artist_id, year, mbid);
        }

        // Le disque a-t-il déjà une entrée, posée par un dossier frère ? Le
        // rangement Qobuz d'une compilation met chaque piste dans le dossier
        // de SON artiste ; sans ce rattrapage, une anthologie de 41 titres
        // produit 41 albums d'une piste.
        if self.find_id_by_folder(folder)?.is_none() {
            if let Some(id) = self.find_scattered_compilation(folder, title, track_number)? {
                let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
                let params: [&dyn ToSqlValue; 1] = [&id];
                if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                    tracing::debug!(
                        album_id = id,
                        folder,
                        title,
                        ?track_number,
                        "scattered_compilation_reattached"
                    );
                    return Ok(row_to_album(&row));
                }
            }
        }

        if let Some(id) = self.find_id_by_folder(folder)? {
            // `_strong` like every other read on this path: the scanner runs
            // inside a `BEGIN IMMEDIATE`, so a pooled reader would not see rows
            // written moments ago by this same transaction.
            let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
            let params: [&dyn ToSqlValue; 1] = [&id];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                let mut album = row_to_album(&row);
                // Un album créé alors que son premier fichier était encore en
                // cours d'écriture (tags illisibles) est retombé sur « Unknown
                // Artist ». Le dossier étant l'identité de l'album, chaque
                // rescan retournait cette ligne telle quelle : l'artiste ne se
                // corrigeait jamais, même une fois toutes les pistes taguées
                // (bug .15 : centaines d'albums « Unknown Artist » dont les
                // pistes portent le bon artiste). On répare ici, au moment où
                // un vrai artiste se présente pour ce dossier.
                self.reclaim_unknown_artist(&mut album, artist_id)?;
                return Ok(album);
            }
        }

        let candidate = self.get_or_create_with_mbid(title, artist_id, year, mbid)?;
        let Some(id) = candidate.id else {
            return Ok(candidate);
        };

        match self.folder_path_of(id)? {
            // Already ours, or freshly created by the call above.
            Some(existing) if existing == folder => Ok(candidate),
            // Another folder owns it: this is a distinct release that merely
            // shares title, artist and year. Give it its own row.
            Some(_) => {
                let create_sql = self.dialect_sql(sql::create_with_mbid, sql::create_with_mbid);
                let params: [&dyn ToSqlValue; 4] = [&title, &artist_id, &year, &mbid];
                let new_id = self.db.execute_returning_id(&create_sql, &params)?;
                self.set_folder_path(new_id, folder)?;
                let mut album = Album::new(title.to_string());
                album.id = Some(new_id);
                album.artist_id = Some(artist_id);
                album.year = year;
                album.musicbrainz_release_id = mbid.map(String::from);
                Ok(album)
            }
            // Indexed before folders were recorded (or just created): claim it.
            None => {
                self.set_folder_path(id, folder)?;
                Ok(candidate)
            }
        }
    }

    /// Rend à l'album son vrai artiste quand il est resté sur « Unknown Artist ».
    ///
    /// Ne touche à rien sauf si TOUTES ces conditions tiennent :
    /// - l'artiste actuel de l'album est « Unknown Artist » (ou `artist_id`
    ///   NULL / ligne artiste disparue) ;
    /// - l'artiste demandé existe et n'est pas lui-même « Unknown Artist ».
    ///
    /// Un album correctement attribué n'est donc jamais réassigné (deux vrais
    /// artistes en désaccord = éditions distinctes, on ne tranche pas ici), et
    /// un fichier encore sans tags ne rétrograde jamais un album déjà résolu.
    fn reclaim_unknown_artist(
        &self,
        album: &mut Album,
        requested_artist_id: i64,
    ) -> Result<(), TuneError> {
        if album.artist_id == Some(requested_artist_id) {
            return Ok(());
        }
        let currently_unknown = match (album.artist_id, album.artist_name.as_deref()) {
            (None, _) => true,
            // artist_id présent mais ligne artiste absente (LEFT JOIN → NULL).
            (Some(_), None) => true,
            (Some(_), Some(name)) => {
                name.eq_ignore_ascii_case(crate::db::artist_repo::UNKNOWN_ARTIST_NAME)
            }
        };
        if !currently_unknown {
            return Ok(());
        }
        let name_sql = self.dialect_sql(sql::get_artist_name, sql::get_artist_name);
        let params: [&dyn ToSqlValue; 1] = [&requested_artist_id];
        let Some(requested_name) = self
            .db
            .query_one_strong(&name_sql, &params)?
            .and_then(|row| row.first()?.as_string())
        else {
            return Ok(());
        };
        if requested_name.eq_ignore_ascii_case(crate::db::artist_repo::UNKNOWN_ARTIST_NAME) {
            return Ok(());
        }
        let Some(album_id) = album.id else {
            return Ok(());
        };
        let set_sql = self.dialect_sql(sql::set_artist_id, sql::set_artist_id);
        let set_params: [&dyn ToSqlValue; 2] = [&requested_artist_id, &album_id];
        self.db.execute(&set_sql, &set_params)?;
        tracing::info!(
            album_id,
            album = %album.title,
            previous_artist = ?album.artist_name,
            new_artist = %requested_name,
            "album_artist_reclaimed_from_unknown"
        );
        album.artist_id = Some(requested_artist_id);
        album.artist_name = Some(requested_name);
        Ok(())
    }

    /// Répare les albums déjà figés sur un artiste partagé par MBID vide.
    ///
    /// Cette passe est destinée à la fin d'un scan complet et sain. Sa requête
    /// est volontairement fail-closed : au moindre artiste de piste divergent,
    /// drapeau compilation ou ALBUMARTIST contradictoire, aucune ligne n'est
    /// candidate. Le retour est le nombre d'albums effectivement réattribués.
    pub fn repair_empty_mbid_artist_collapses(&self) -> Result<usize, TuneError> {
        let candidate_sql = sql::empty_mbid_artist_collapse_candidates();
        let update_sql = self.dialect_sql(sql::set_artist_id, sql::set_artist_id);
        let mut repaired = 0usize;

        self.db.write_tx(&mut |tx| {
            let candidates = tx.query_many(candidate_sql, &[])?;
            for row in candidates {
                let Some(album_id) = row.first().and_then(|value| value.as_i64()) else {
                    continue;
                };
                let title = row
                    .get(1)
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                let previous_artist_id = row.get(2).and_then(|value| value.as_i64());
                let Some(target_artist_id) = row.get(3).and_then(|value| value.as_i64()) else {
                    continue;
                };
                let target_name = row
                    .get(4)
                    .and_then(|value| value.as_string())
                    .unwrap_or_default();
                if target_name
                    .trim()
                    .eq_ignore_ascii_case(crate::db::artist_repo::UNKNOWN_ARTIST_NAME)
                {
                    continue;
                }

                let params: [&dyn ToSqlValue; 2] = [&target_artist_id, &album_id];
                if tx.execute(&update_sql, &params)? > 0 {
                    repaired += 1;
                    tracing::warn!(
                        album_id,
                        album = %title,
                        previous_artist_id = ?previous_artist_id,
                        target_artist_id,
                        target_artist = %target_name,
                        "album_artist_repaired_empty_mbid_collapse"
                    );
                }
            }
            Ok(())
        })?;

        Ok(repaired)
    }

    /// The id of the album a folder holds, if one is recorded.
    /// L'album auquel rattacher une piste dont le dossier est l'éclat d'une
    /// compilation rangée par artiste (#1440).
    ///
    /// Trois conditions doivent tenir ENSEMBLE, et la troisième est celle qui
    /// protège les homonymes : même titre, dossiers frères éparpillés (voir
    /// [`crate::scanner::compilation::is_scattered_sibling`]), et un numéro de
    /// piste encore libre dans l'album candidat. Deux « Greatest Hits »
    /// distincts commencent tous deux à la piste 1 : ils ne fusionnent pas.
    pub fn find_scattered_compilation(
        &self,
        folder: &str,
        title: &str,
        track_number: Option<i32>,
    ) -> Result<Option<i64>, TuneError> {
        use crate::scanner::compilation::{
            folder_cover_fingerprint, is_scattered_sibling, track_number_is_free,
        };
        if track_number.is_none() || folder.is_empty() {
            return Ok(None);
        }
        // La pochette du dossier est le SEUL signal qui identifie un disque.
        //
        // Le numéro de piste libre ne suffit pas au scan : l'album se remplit
        // progressivement, donc la piste 1 d'un second volume arrive quand
        // l'album ne contient encore que les pistes 5, 12, 18 du premier — elle
        // est absorbée, et de proche en proche les volumes se confondent.
        // Constaté sur .18 : les quatre volumes « ALLOPOP » écrasés en un seul
        // album de 71 pistes, chaque numéro en quatre exemplaires.
        //
        // Sans pochette on renonce : mieux vaut deux albums de trop qu'un
        // disque avalé par un autre.
        let Some(empreinte) = folder_cover_fingerprint(folder) else {
            return Ok(None);
        };
        let sql = self.dialect_sql(sql::scattered_candidates, sql::scattered_candidates);
        let params: [&dyn ToSqlValue; 2] = [&title, &folder];
        for row in self.db.query_many_strong(&sql, &params)? {
            let Some(id) = row.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let Some(cand_folder) = row.get(1).and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            if !is_scattered_sibling(folder, &cand_folder) {
                continue;
            }
            // Même titre, dossiers frères — mais est-ce le MÊME disque ?
            if !folder_cover_fingerprint(&cand_folder).is_some_and(|f| f.matches(&empreinte)) {
                continue;
            }
            let taken: Vec<i32> = row
                .get(2)
                .and_then(|v| v.as_string())
                .map(|csv| {
                    csv.split(',')
                        .filter_map(|n| n.trim().parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            if track_number_is_free(track_number, &taken) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub fn find_id_by_folder(&self, folder: &str) -> Result<Option<i64>, TuneError> {
        let sql = self.dialect_sql(sql::get_id_by_folder, sql::get_id_by_folder);
        let params: [&dyn ToSqlValue; 1] = [&folder];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .and_then(|row| row.first()?.as_i64()))
    }

    /// The folder recorded for an album, `None` when it predates the column.
    pub fn folder_path_of(&self, album_id: i64) -> Result<Option<String>, TuneError> {
        let sql = self.dialect_sql(sql::get_folder_path, sql::get_folder_path);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .and_then(|row| row.first()?.as_string())
            .filter(|s| !s.is_empty()))
    }

    pub fn set_folder_path(&self, album_id: i64, folder: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::set_folder_path, sql::set_folder_path);
        let params: [&dyn ToSqlValue; 2] = [&folder, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Marque l'album comme compilation (#1957). **Ne baisse jamais le
    /// drapeau**, et c'est délibéré :
    ///
    /// - le scan voit les pistes une par une ; la première d'une anthologie
    ///   peut, seule, ne rien avoir de « compilation ». Un `SET = <décision>`
    ///   ferait dépendre le résultat de l'ordre d'arrivée des fichiers ;
    /// - le regroupement lui-même est déjà irréversible dans les faits :
    ///   l'album a pris « Various Artists » pour artiste et le rescan ne le lui
    ///   reprend pas. Le drapeau décrit ce regroupement — il doit avoir la même
    ///   durée de vie, sans quoi la pastille contredirait l'écran.
    ///
    /// La voie de réparation d'un faux positif reste le rescan complet, qui
    /// repart d'un `DELETE FROM albums` (`track_repo::delete_all`) : les lignes
    /// sont reconstruites, drapeau compris, d'après les tags du moment.
    ///
    /// Idempotent : la clause `COALESCE(is_compilation, 0) = 0` fait de tout
    /// appel suivant un no-op, donc aucun coût sur un rescan.
    pub fn mark_compilation(&self, album_id: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::mark_compilation, sql::mark_compilation);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Like `get_by_title_and_artist` but uses `query_one_strong` to
    /// read through the write connection. Called by `get_or_create` /
    /// `get_or_create_with_mbid` which run inside a scanner
    /// `BEGIN IMMEDIATE` transaction.
    fn find_by_title_and_artist_strong(
        &self,
        title: &str,
        artist_id: i64,
        year: Option<i32>,
    ) -> Result<Option<Album>, TuneError> {
        if let Some(y) = year {
            let sql =
                self.dialect_sql(sql::get_by_title_artist_year, sql::get_by_title_artist_year);
            let params: [&dyn ToSqlValue; 3] = [&title, &artist_id, &y];
            if let Some(row) = self.db.query_one_strong(&sql, &params)? {
                return Ok(Some(row_to_album(&row)));
            }
        }
        let sql = self.dialect_sql(sql::get_by_title_artist, sql::get_by_title_artist);
        let params: [&dyn ToSqlValue; 2] = [&title, &artist_id];
        Ok(self
            .db
            .query_one_strong(&sql, &params)?
            .as_ref()
            .map(row_to_album))
    }

    pub fn update(&self, album: &Album) -> Result<(), TuneError> {
        let id = album.id.ok_or("album has no id")?;
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 20] = [
            &album.title,
            &album.artist_id,
            &album.year,
            &album.original_year,
            &album.genre,
            &album.genres,
            &album.disc_count,
            &album.track_count,
            &album.cover_path,
            &album.label,
            &album.catalog_number,
            &album.format,
            &album.sample_rate,
            &album.bit_depth,
            &album.bio,
            &album.musicbrainz_release_id,
            &album.musicbrainz_release_group_id,
            &album.release_date,
            &album.original_date,
            &id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Set album date fields (original_year, release_date, original_date)
    /// using COALESCE — only fills in values not already set.
    pub fn update_dates(
        &self,
        album_id: i64,
        year: Option<i32>,
        original_year: Option<i32>,
        release_date: Option<&str>,
        original_date: Option<&str>,
    ) -> Result<(), TuneError> {
        // Skip if all values are None — nothing to update.
        if year.is_none()
            && original_year.is_none()
            && release_date.is_none()
            && original_date.is_none()
        {
            return Ok(());
        }
        let sql = self.dialect_sql(sql::update_dates, sql::update_dates);
        let params: [&dyn ToSqlValue; 5] = [
            &year,
            &original_year,
            &release_date,
            &original_date,
            &album_id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Ecrit l'annee en ecrasant celle en place — voir `sql::set_year`.
    pub fn set_year(&self, album_id: i64, year: i32) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::set_year, sql::set_year);
        let params: [&dyn ToSqlValue; 2] = [&year, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_cover_path(&self, album_id: i64, cover_path: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_cover_path, sql::update_cover_path);
        let params: [&dyn ToSqlValue; 2] = [&cover_path, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Like `update_cover_path` but always overwrites the existing value.
    /// Used by rescan endpoints where the user explicitly wants to refresh artwork.
    pub fn force_update_cover_path(
        &self,
        album_id: i64,
        cover_path: &str,
    ) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::force_update_cover_path, sql::force_update_cover_path);
        let params: [&dyn ToSqlValue; 2] = [&cover_path, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_track_count(&self, album_id: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_track_count, sql::update_track_count);
        let params: [&dyn ToSqlValue; 2] = [&album_id, &album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_quality_from_tracks(&self, album_id: i64) -> Result<(), TuneError> {
        // 7 references to the same album_id parameter. SQLite uses `?`
        // for each; PG would use $1..$7 — we build the placeholder list
        // via the dialect to keep both engines happy.
        let p = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let plist = (1..=7)
            .map(|i| match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(i),
                Engine::Postgres => PostgresDialect.placeholder(i),
            })
            .collect::<Vec<_>>();
        let _ = p;
        // genre/genres are fill-only (COALESCE) but must heal the empty-string
        // case: `create_minimal` leaves genre NULL, yet a track can carry
        // genre = '' (an empty tag frame), which the old `t.genre IS NOT NULL`
        // subquery would happily pick and COALESCE into the album — pinning it
        // to '' forever (COALESCE(albums.genre='' , …) never re-fills, and the
        // completeness card counts genre != '' so it read as "without genre" for
        // the whole catalogue, identical to the cover card — #3 Fabien). NULLIF
        // treats a stored '' as re-fillable and the `!= ''` guard only ever
        // sources a real, non-empty genre. Valid on SQLite and PostgreSQL.
        let sql = format!(
            "UPDATE albums SET
                format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = {} AND t.format IS NOT NULL LIMIT 1)),
                sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = {})),
                bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = {})),
                genre = COALESCE(NULLIF(albums.genre, ''), (SELECT t.genre FROM tracks t WHERE t.album_id = {} AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1)),
                genres = COALESCE(NULLIF(albums.genres, ''), (SELECT t.genres FROM tracks t WHERE t.album_id = {} AND t.genres IS NOT NULL AND t.genres != '' LIMIT 1)),
                disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = {}))
            WHERE id = {}",
            plist[0], plist[1], plist[2], plist[3], plist[4], plist[5], plist[6]
        );
        let params: [&dyn ToSqlValue; 7] = [
            &album_id, &album_id, &album_id, &album_id, &album_id, &album_id, &album_id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete_orphans(&self) -> Result<i64, TuneError> {
        let mut count: i64 = 0;
        let count_ref = &mut count;
        self.db.write_tx(&mut |tx| {
            *count_ref = tx
                .query_one(sql::count_orphans(), &[])?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(0);
            if *count_ref > 0 {
                tx.execute(sql::delete_orphans(), &[])?;
            }
            Ok(())
        })?;
        Ok(count)
    }

    pub fn count(&self) -> Result<i64, TuneError> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    /// Compteur de pagination de la grille d'albums : exclut les masqués,
    /// comme la liste qu'il pagine (#1391). `count()` reste le compte COMPLET
    /// (stats, maintenance).
    pub fn count_visible(&self) -> Result<i64, TuneError> {
        match self.db.query_one(&sql::count_visible(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn count_with_bio(&self) -> Result<i64, TuneError> {
        match self.db.query_one(sql::count_with_bio(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    /// Return all albums that have both a bio and a MusicBrainz release group ID.
    /// Each entry is (title, artist_name, musicbrainz_release_group_id, bio).
    pub fn albums_with_bio_and_mbid(
        &self,
    ) -> Result<Vec<(String, Option<String>, String, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_with_bio_and_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                    cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Return all albums that have a non-empty bio, regardless of MBID.
    /// Each entry is (title, artist_name, musicbrainz_release_group_id, bio).
    /// The MBID may be None for albums without a MusicBrainz ID.
    #[allow(clippy::type_complexity)]
    pub fn albums_with_bio(
        &self,
    ) -> Result<
        Vec<(
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
        TuneError,
    > {
        let rows = self.db.query_many(sql::list_with_bio(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                    cols.get(3).and_then(|v| v.as_string()),
                    cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(5).and_then(|v| v.as_string()),
                    cols.get(6).and_then(|v| v.as_string()),
                    cols.get(7).and_then(|v| v.as_string()),
                    cols.get(8).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    /// Return all albums that have a MusicBrainz release group ID but no local bio.
    /// Used by the community bio download to find candidates for enrichment.
    /// Each entry is (album_id, musicbrainz_release_group_id).
    pub fn albums_without_bio_with_mbid(&self) -> Result<Vec<(i64, String)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_bio_with_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Return all local albums without bio and without MBID.
    /// Used by the community bio download to find candidates for title+artist lookup.
    /// Each entry is (album_id, title, artist_name).
    pub fn albums_without_bio_without_mbid(
        &self,
    ) -> Result<Vec<(i64, String, Option<String>)>, TuneError> {
        let rows = self
            .db
            .query_many(sql::list_without_bio_without_mbid(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    pub fn list_recent(&self, limit: i64) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_recent, sql::list_recent);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    pub fn list_by_release_group(&self, group_id: &str) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_release_group, sql::list_by_release_group);
        let params: [&dyn ToSqlValue; 1] = [&group_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    pub fn list_release_groups(&self) -> Result<Vec<(String, Vec<Album>)>, TuneError> {
        let rows = self.db.query_many(&sql::list_release_groups(), &[])?;
        let albums: Vec<Album> = rows.iter().map(row_to_album).collect();

        let mut groups: std::collections::HashMap<String, Vec<Album>> =
            std::collections::HashMap::new();
        for album in albums {
            if let Some(ref gid) = album.musicbrainz_release_group_id {
                groups.entry(gid.clone()).or_default().push(album);
            }
        }
        Ok(groups.into_iter().filter(|(_, v)| v.len() > 1).collect())
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Album>, TuneError> {
        self.list_sorted(limit, offset, "title", "asc")
    }

    pub fn list_sorted(
        &self,
        limit: i64,
        offset: i64,
        sort: &str,
        order: &str,
    ) -> Result<Vec<Album>, TuneError> {
        // `include_hidden = true` : `list()`/`list_sorted` servent la
        // MAINTENANCE (rattrapage de pochettes du scan, export complet), qui
        // doit voir toute la bibliothèque, masqués compris. La grille passe
        // par `list_filtered` et choisit.
        self.list_filtered(limit, offset, sort, order, None, None, None, true, None)
    }

    /// Date d'ajout de CHAQUE album, calculée en UNE passe sur `tracks` (jointure
    /// groupée) et exposée sous l'alias `aa.added_at` (#1269).
    ///
    /// Type de la valeur : `file_first_seen.first_seen_at` is DOUBLE, but the
    /// type of `tracks.file_mtime` on Postgres depends on the install vintage:
    /// TEXT on some installs (bug #550 fixed that case), DOUBLE PRECISION on
    /// others (.15) — schema drift between PG installs. A bare
    /// COALESCE(double, text) is a hard error on the TEXT installs, and
    /// NULLIF(double_col, '') is a hard error at *parse/analyze* time on the
    /// DOUBLE installs ("invalid input syntax for type double precision"),
    /// which made the sort return no albums on .15 (empty library → "black
    /// screen" on the clients) — and added_at is the handler's default sort,
    /// so every list was empty. Cast the column to TEXT first so the
    /// expression is valid whichever type the column has; NULLIF guards
    /// against empty strings. Valid on SQLite too (soft affinities).
    pub(crate) const ADDED_AT_JOIN: &'static str = "LEFT JOIN (SELECT t.album_id, \
                MAX(COALESCE(ffs.first_seen_at, CAST(NULLIF(CAST(t.file_mtime AS TEXT), '') AS DOUBLE PRECISION))) AS added_at \
           FROM tracks t LEFT JOIN file_first_seen ffs ON ffs.file_path = t.file_path \
           GROUP BY t.album_id) aa ON aa.album_id = a.id";

    /// Jointure GROUPÉE qui donne le Dynamic Range de CHAQUE album en une
    /// passe, exposé sous l'alias `dr.dr` (#2144).
    ///
    /// Même forme que [`Self::ADDED_AT_JOIN`], et pour la même raison : une
    /// sous-requête corrélée (`(SELECT … WHERE t.album_id = a.id)`) serait
    /// ré-évaluée POUR CHAQUE LIGNE du tri, c'est-à-dire 45 000 fois sur la
    /// bibliothèque de Megalo — exactement le coût que #1269 vient de retirer.
    ///
    /// Le tag vit en TEXT dans `track_metadata` (`dr_album`, écrit par le scan
    /// depuis `ALBUM DYNAMIC RANGE`, #1806). `normalise_dr` retire déjà le
    /// préfixe et les zéros de tête, mais il RECOPIE TEL QUEL tout ce qui n'est
    /// pas une suite de chiffres (« DR12.5 », un commentaire, une valeur
    /// tronquée) : sans garde, `CAST('DR12.5' AS INTEGER)` vaut 0 en SQLite —
    /// un master saturé de plus, silencieusement — et **échoue durement** en
    /// PostgreSQL (`invalid input syntax for type integer`), ce qui viderait la
    /// grille entière. D'où le prédicat « que des chiffres », écrit dans le
    /// dialecte de chaque moteur, AVANT le CAST.
    ///
    /// `LENGTH(tm.value) <= 3` pour la même raison, un cran plus loin : « que
    /// des chiffres » n'empêche pas `99999999999999999999`, qui déborde
    /// l'`INTEGER` de PostgreSQL (`out of range`) et ferait tomber la requête
    /// entière — donc la grille — sur UN seul tag corrompu. Un DR se mesure
    /// entre 0 et une vingtaine ; trois chiffres sont déjà très généreux.
    ///
    /// `MAX` plutôt qu'un `LIMIT 1` : le tag décrit l'album mais vit dans
    /// chaque piste, donc n'importe laquelle répond — encore faut-il qu'elle
    /// réponde TOUJOURS LA MÊME, sans quoi deux pages successives trieraient
    /// le même album à deux places différentes.
    pub(crate) fn dr_album_join(engine: Engine) -> String {
        let only_digits = match engine {
            // SQLite n'a pas de `~` ; GLOB est son motif sensible à la casse,
            // et `[^0-9]` y est une classe de caractères niée.
            Engine::Sqlite => "tm.value NOT GLOB '*[^0-9]*'",
            Engine::Postgres => "tm.value ~ '^[0-9]+$'",
        };
        format!(
            "LEFT JOIN (SELECT t.album_id AS album_id, MAX(CAST(tm.value AS INTEGER)) AS dr \
               FROM track_metadata tm JOIN tracks t ON t.id = tm.track_id \
              WHERE tm.key = 'dr_album' AND tm.value <> '' \
                AND LENGTH(tm.value) <= 3 AND {only_digits} \
              GROUP BY t.album_id) dr ON dr.album_id = a.id"
        )
    }

    /// Prédicat « l'album porte un DR compris dans la tranche », posé sur
    /// l'alias de [`Self::dr_album_join`]. Les bornes sont liées par
    /// l'appelant, dans l'ordre où les marqueurs sont demandés.
    fn dr_wheres(
        range: DrRange,
        make_ph: &dyn Fn(usize) -> String,
        next_ph: &mut usize,
        binds: &mut Vec<SqlValue>,
    ) -> Vec<String> {
        // `IS NOT NULL` explicite : un album sans tag NE DOIT PAS passer un
        // filtre de tranche. Les comparaisons suivantes l'excluraient déjà
        // (NULL >= 8 vaut NULL, donc pas vrai), mais une tranche ouverte des
        // deux côtés n'en poserait aucune — et rendrait alors la bibliothèque
        // entière au lieu des seuls albums tagués.
        let mut out = vec!["dr.dr IS NOT NULL".to_string()];
        if let Some(min) = range.min {
            out.push(format!("dr.dr >= {}", make_ph(*next_ph)));
            *next_ph += 1;
            binds.push(SqlValue::Int(min));
        }
        if let Some(max) = range.max {
            out.push(format!("dr.dr <= {}", make_ph(*next_ph)));
            *next_ph += 1;
            binds.push(SqlValue::Int(max));
        }
        out
    }

    /// Effectif de la tranche de DR demandée (#2144) — le `total` que la
    /// grille pagine.
    ///
    /// Sans lui, `count_visible()` annoncerait toute la bibliothèque alors que
    /// la liste ne rend que les albums tagués : la grille afficherait des
    /// centaines de pages vides, puisque le DR n'est tagué que sur une part
    /// infime des bibliothèques (Bertrand, 15/08 : « ça suppose que les tags
    /// soient présents sur une part suffisante de la bibliothèque »).
    pub fn count_in_dr_range(
        &self,
        range: DrRange,
        include_hidden: bool,
    ) -> Result<i64, TuneError> {
        let engine = self.db.engine();
        let make_ph = |i: usize| match engine {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let mut next_ph = 1usize;
        let mut binds: Vec<SqlValue> = Vec::new();
        let mut wheres = Self::dr_wheres(range, &make_ph, &mut next_ph, &mut binds);
        if !include_hidden {
            wheres.push(crate::db::facet_filter::hidden_albums_excluded().to_string());
        }
        let sql = format!(
            "SELECT COUNT(*) FROM albums a {} WHERE {}",
            Self::dr_album_join(engine),
            wheres.join(" AND ")
        );
        let refs: Vec<&dyn ToSqlValue> = binds.iter().map(|v| v as &dyn ToSqlValue).collect();
        Ok(self
            .db
            .query_one(&sql, &refs)?
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }

    /// Les valeurs de Dynamic Range RÉELLEMENT présentes dans la
    /// bibliothèque, croissantes (#2144).
    ///
    /// C'est la matière des tranches : l'issue ne fixe aucune borne et les
    /// bornes exactes de MinimServer, citées en modèle, n'ont jamais été
    /// relevées. Le serveur n'en invente donc pas — il dit ce qu'il a, et le
    /// client découpe. Une bibliothèque sans aucun tag rend une liste vide,
    /// et l'écran n'affiche pas de facette plutôt qu'une facette morte.
    pub fn dynamic_range_values(&self) -> Result<Vec<i64>, TuneError> {
        let engine = self.db.engine();
        let sql = format!(
            "SELECT DISTINCT dr.dr FROM albums a {} WHERE dr.dr IS NOT NULL AND {} ORDER BY dr.dr",
            Self::dr_album_join(engine),
            crate::db::facet_filter::hidden_albums_excluded()
        );
        Ok(self
            .db
            .query_many(&sql, &[])?
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn list_filtered(
        &self,
        limit: i64,
        offset: i64,
        sort: &str,
        order: &str,
        format: Option<&str>,
        quality: Option<&str>,
        // `Some(true)` = seulement les compilations, `Some(false)` = tout sauf
        // elles, `None` = pas de filtre (#1957).
        compilation: Option<bool>,
        // `false` (le défaut des routes) = les albums masqués sont exclus ;
        // `true` = tout rendre, pour `?include_hidden=true` et les appels de
        // maintenance (#1391).
        include_hidden: bool,
        // Tranche de Dynamic Range (#2144). `None` = pas de filtre, et alors
        // AUCUNE jointure supplémentaire n'est ajoutée : le SQL du cas courant
        // est exactement celui d'avant.
        dr: Option<DrRange>,
    ) -> Result<Vec<Album>, TuneError> {
        let dir = if order.eq_ignore_ascii_case("desc") {
            "DESC"
        } else {
            "ASC"
        };
        // `a.id` en DERNIER départage de chaque tri : sans clé unique finale,
        // l'ordre des égalités peut changer d'une requête à l'autre, et toute
        // pagination (le Browse UPnP page par 200) SAUTE des albums et en
        // double d'autres — six albums absents de la grille d'un serveur Tune
        // distant (jeu des sept erreurs, 25/08).
        let order_clause = match sort {
            "title" => format!("LOWER(a.title) {dir}, a.id ASC"),
            "release_date" => format!(
                "COALESCE(a.release_date, a.original_date, CAST(a.year AS TEXT)) {dir} NULLS LAST, LOWER(a.title) ASC, a.id ASC"
            ),
            // The web client's sort dropdown labels this option "original_year"
            // (LibraryView AlbumSortKey); accept it as an alias for "year" so an
            // unknown key doesn't silently fall through to the `a.id` default.
            "year" | "original_year" => {
                format!("a.year {dir} NULLS LAST, LOWER(a.title) ASC, a.id ASC")
            }
            "artist" => {
                format!("LOWER(ar.name) {dir}, a.year ASC, LOWER(a.title) ASC, a.id ASC")
            }
            // "Date added" must survive a full rescan. A full rescan does
            // `DELETE FROM albums` + reinsert (track_repo::delete_all), so
            // album ids are reassigned in filesystem-walk order — sorting by
            // `a.id` reflects the walk order, not when files were added, and
            // file mtime is unreliable for bulk-copied NAS libraries (eric:
            // "tri par date d'ajout fantaisiste après rescan"). Use the
            // persistent `file_first_seen` timestamp (recorded once per path,
            // never purged by delete_all), falling back to file mtime for any
            // track not yet recorded. Streaming albums have no local file →
            // NULLS LAST, id tiebreaker.
            // "added_date" is the web client's key (LibraryView AlbumSortKey) for
            // this same option; alias it so "sort by date added" actually sorts by
            // date rather than silently falling through to the `a.id` default —
            // which only *looks* correct for the most-recently-added albums (their
            // ids happen to be the highest), hence the "only the first few albums
            // are sorted" report (Bilou, #1102).
            // The timestamp itself comes from `ADDED_AT_JOIN`, computed ONCE
            // for the whole page — see the comment on that constant (#1269).
            "added_at" | "added_date" => format!("aa.added_at {dir} NULLS LAST, a.id {dir}"),
            // Dynamic Range (#2144). `NULLS LAST` dans LES DEUX sens : un
            // album sans tag n'a pas un DR bas, il n'en a pas — le ranger avec
            // les masters saturés serait un mensonge, et le testeur qui trie
            // par DR croissant tomberait d'abord sur les albums qu'il n'a pas
            // tagués. Départage par titre pour que la pagination soit stable.
            "dynamic_range" | "dr" => {
                format!("dr.dr {dir} NULLS LAST, LOWER(a.title) ASC, a.id ASC")
            }
            _ => format!("a.id {dir}"),
        };

        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };

        let mut wheres: Vec<String> = Vec::new();
        let mut bind_values: Vec<SqlValue> = Vec::new();
        let mut next_ph = 1usize;

        if let Some(fmt) = format {
            wheres.push(format!(
                "a.id IN (SELECT DISTINCT album_id FROM tracks WHERE format = {})",
                make_ph(next_ph)
            ));
            bind_values.push(SqlValue::Text(fmt.to_string()));
            next_ph += 1;
        }
        match quality {
            Some("dsd") => {
                wheres.push("a.id IN (SELECT DISTINCT album_id FROM tracks WHERE format IN ('dsd','dsf','dff'))".to_string());
            }
            Some("hires") => {
                wheres.push("a.id IN (SELECT DISTINCT album_id FROM tracks WHERE sample_rate > 44100 OR bit_depth > 16)".to_string());
            }
            Some("cd") => {
                wheres.push("(a.sample_rate = 44100 AND a.bit_depth = 16)".to_string());
            }
            Some("lossy") => {
                wheres.push("a.format IN ('mp3','aac','ogg','opus','wma')".to_string());
            }
            _ => {}
        }
        // `COALESCE` : une base migrée depuis SQLite peut porter des NULL, et
        // `NULL = 0` vaut NULL — sans lui, « tout sauf les compilations »
        // masquerait aussi les albums jamais rescannés.
        match compilation {
            Some(true) => wheres.push("COALESCE(a.is_compilation, 0) <> 0".to_string()),
            Some(false) => wheres.push("COALESCE(a.is_compilation, 0) = 0".to_string()),
            None => {}
        }
        // Albums masqués (#1391) : exclus par défaut. Le prédicat passe par
        // `wheres` — jamais par le `replacen` de `base_select` plus bas, qui
        // ne réécrit que la liste de colonnes.
        if !include_hidden {
            wheres.push(crate::db::facet_filter::hidden_albums_excluded().to_string());
        }
        // Tranche de DR (#2144) : les marqueurs se prennent ICI, avant ceux de
        // LIMIT/OFFSET, sinon PostgreSQL décale toutes les valeurs liées (le
        // `?` de SQLite, lui, ignore l'indice et masquerait le défaut — piège
        // n°2 de `facet_filter`).
        let dr_sort = matches!(sort, "dynamic_range" | "dr");
        if let Some(range) = dr {
            wheres.extend(Self::dr_wheres(
                range,
                &make_ph,
                &mut next_ph,
                &mut bind_values,
            ));
        }

        let where_clause = if wheres.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wheres.join(" AND "))
        };

        let limit_ph = make_ph(next_ph);
        next_ph += 1;
        let offset_ph = make_ph(next_ph);

        // #1269 — deux temps, parce qu'une seule requête triait des lignes
        // COMPLÈTES : 45 000 albums × 25 colonnes (dont `bio`, plusieurs Ko
        // sur une bibliothèque enrichie) traversaient le trieur À CHAQUE page,
        // et le tri par défaut (`added_at`) y ajoutait une sous-requête
        // corrélée ré-évaluée par ligne. Les clients iOS/macOS chargent tout
        // par pages de 2000 avec 15 s de timeout par requête : sur la
        // bibliothèque de Megalo (~45 000 albums), la grille restait à
        // 0 album (forum p.16).
        //
        // 1er temps : trier des lignes ÉTROITES — a.id et la clé de tri
        // seulement — et borner en SQL (LIMIT/OFFSET).
        let added_at_sort = matches!(sort, "added_at" | "added_date");
        let mut joins = "LEFT JOIN artists ar ON a.artist_id = ar.id".to_string();
        if added_at_sort {
            // `aa.added_at` vient de la jointure groupée `ADDED_AT_JOIN` —
            // une seule passe sur tracks/file_first_seen, exposée en 2e
            // colonne pour que le client puisse rendre sa frise chronologique.
            joins.push(' ');
            joins.push_str(Self::ADDED_AT_JOIN);
        }
        // La jointure DR n'est posée QUE si on trie ou filtre dessus : le
        // listage par défaut garde le SQL — et le plan — de #1269 au caractère
        // près.
        if dr_sort || dr.is_some() {
            joins.push(' ');
            joins.push_str(&Self::dr_album_join(self.db.engine()));
        }
        let id_select = if added_at_sort {
            format!("SELECT a.id, aa.added_at FROM albums a {joins}")
        } else {
            format!("SELECT a.id FROM albums a {joins}")
        };
        let sql = format!(
            "{id_select}{where_clause} ORDER BY {order_clause} LIMIT {limit_ph} OFFSET {offset_ph}"
        );

        bind_values.push(SqlValue::Int(limit));
        bind_values.push(SqlValue::Int(offset));

        let refs: Vec<&dyn ToSqlValue> = bind_values.iter().map(|v| v as &dyn ToSqlValue).collect();
        let rows = self.db.query_many(&sql, &refs)?;
        let ordered_ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect();
        let added_at_by_id: std::collections::HashMap<i64, f64> = if added_at_sort {
            rows.iter()
                .filter_map(|r| {
                    Some((
                        r.first().and_then(|v| v.as_i64())?,
                        r.get(1).and_then(|v| v.as_f64())?,
                    ))
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        };
        if ordered_ids.is_empty() {
            return Ok(Vec::new());
        }

        // 2e temps : ne matérialiser QUE la page. Ids issus de la base (i64),
        // inlinés sans placeholder — par tranches, pour rester sous la
        // limite de longueur SQL de SQLite (1 Mo) même à `limit` géant.
        let mut by_id: std::collections::HashMap<i64, Album> =
            std::collections::HashMap::with_capacity(ordered_ids.len());
        for chunk in ordered_ids.chunks(5000) {
            let id_list = chunk
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("{} WHERE a.id IN ({id_list})", sql::select_album());
            let rows = self.db.query_many(&sql, &[])?;
            for row in &rows {
                let album = row_to_album(row);
                if let Some(id) = album.id {
                    by_id.insert(id, album);
                }
            }
        }
        Ok(ordered_ids
            .iter()
            .filter_map(|id| {
                let mut album = by_id.remove(id)?;
                album.added_at = added_at_by_id.get(id).copied();
                Some(album)
            })
            .collect())
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_artist, sql::list_by_artist);
        let params: [&dyn ToSqlValue; 1] = [&artist_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    /// Les albums d'une année (colonne `year`), triés par titre.
    ///
    /// C'est la requête du conteneur UPnP « Years », comme `list_by_genre`
    /// est celle de « Genres » : le serveur média ne doit pas inventer sa
    /// propre lecture de la colonne.
    pub fn list_by_year(&self, year: i64) -> Result<Vec<Album>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_year, sql::list_by_year);
        let params: [&dyn ToSqlValue; 1] = [&year];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    /// When `album_id` points at an album row that has NO tracks (a stale or
    /// duplicate row), find another album with the SAME title + artist that DOES
    /// have tracks — the row the Artists view reaches. Returns the sibling id
    /// with the most tracks, or `None` if there is no populated sibling.
    ///
    /// This is why the same album could 400 ("no tracks to play") from the flat
    /// Albums/Genres/Years grids (which can surface the empty row) yet play fine
    /// from the Artists view (which reaches the populated row) — Pascal, v0.9.21.
    pub fn find_populated_sibling(&self, album_id: i64) -> Result<Option<i64>, TuneError> {
        let ph = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!(
            "SELECT a.id FROM albums a \
             JOIN albums s ON s.id = {ph} \
             WHERE a.id <> s.id \
               AND LOWER(a.title) = LOWER(s.title) \
               AND ((a.artist_id = s.artist_id) OR (a.artist_id IS NULL AND s.artist_id IS NULL)) \
               AND EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id) \
             ORDER BY (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id) DESC \
             LIMIT 1"
        );
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64()))
    }

    /// Match albums where `genre` appears in either the legacy
    /// delimiter-separated text column or the structured `genres`
    /// JSON array (via `dialect.json_array_contains_lower`). Now
    /// PG-compatible.
    pub fn list_by_genre(&self, genre: &str) -> Result<Vec<Album>, TuneError> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let json_contains = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.json_array_contains_lower("a.genres", &make_ph(2)),
            Engine::Postgres => PostgresDialect.json_array_contains_lower("a.genres", &make_ph(2)),
        };
        let delimited_pattern = format!("%,{},%", genre.replace('%', "").replace('_', ""));
        let sql = format!(
            "{} WHERE (\
             LOWER(',' || REPLACE(REPLACE(REPLACE(REPLACE(a.genre, '; ', ','), ';', ','), '/ ', ','), '/', ',') || ',') LIKE LOWER({}) \
             OR {}) \
             AND {} \
             ORDER BY LOWER(a.title)",
            sql::select_album(),
            make_ph(1),
            json_contains,
            crate::db::facet_filter::hidden_albums_excluded(),
        );
        let params: [&dyn ToSqlValue; 2] = [&delimited_pattern, &genre];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }

    /// Return all local albums that have no cover art set.
    /// Each entry is (album_id, title, artist_name, musicbrainz_release_id).
    #[allow(clippy::type_complexity)]
    pub fn list_without_cover(
        &self,
    ) -> Result<Vec<(i64, String, Option<String>, Option<String>)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_cover(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                    cols.get(3).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    /// Return all local albums without bio.
    /// Each entry is (album_id, title, artist_name).
    pub fn list_without_bio(&self) -> Result<Vec<(i64, String, Option<String>)>, TuneError> {
        let rows = self.db.query_many(sql::list_without_bio(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()),
                )
            })
            .collect())
    }

    pub fn update_bio(&self, album_id: i64, bio: &str) -> Result<(), TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => "UPDATE albums SET bio = ? WHERE id = ?",
            Engine::Postgres => "UPDATE albums SET bio = $1 WHERE id = $2",
        };
        let params: [&dyn ToSqlValue; 2] = [&bio, &album_id];
        self.db.execute(sql, &params)?;
        Ok(())
    }

    /// Update bio together with its provenance (source, URL, license, lang) and
    /// stamp the fetch time. Needed for CC BY-SA attribution and freshness.
    pub fn update_bio_full(
        &self,
        album_id: i64,
        bio: &str,
        source: &str,
        source_url: Option<String>,
        license: &str,
        lang: &str,
    ) -> Result<(), TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => {
                "UPDATE albums SET bio = ?, bio_source = ?, bio_source_url = ?, \
                 bio_license = ?, bio_lang = ?, bio_fetched_at = CURRENT_TIMESTAMP WHERE id = ?"
            }
            Engine::Postgres => {
                "UPDATE albums SET bio = $1, bio_source = $2, bio_source_url = $3, \
                 bio_license = $4, bio_lang = $5, bio_fetched_at = CURRENT_TIMESTAMP WHERE id = $6"
            }
        };
        let params: [&dyn ToSqlValue; 6] = [&bio, &source, &source_url, &license, &lang, &album_id];
        self.db.execute(sql, &params)?;
        Ok(())
    }

    /// Bio provenance (source, url, license, lang, fetched_at) for the
    /// album-detail endpoint. Returns None when no sourced bio is recorded.
    /// The album's Dynamic Range, as tagged by an external analyser.
    ///
    /// The value is written per track by the scanner (`track_metadata['dr_album']`,
    /// read from the Vorbis `ALBUM DYNAMIC RANGE` field) because that is where
    /// the tag physically lives — in each file — while it describes the album as
    /// a whole. Any one track therefore answers for the album, hence `LIMIT 1`.
    ///
    /// Returns `None` when no track carries the tag, which is the common case:
    /// tagging DR is a deliberate step most libraries never take. The caller
    /// must render nothing at all rather than an empty field.
    pub fn dynamic_range(&self, id: i64) -> Result<Option<String>, TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => {
                "SELECT tm.value FROM track_metadata tm \
                 JOIN tracks t ON t.id = tm.track_id \
                 WHERE t.album_id = ? AND tm.key = 'dr_album' AND tm.value <> '' \
                 LIMIT 1"
            }
            Engine::Postgres => {
                "SELECT tm.value FROM track_metadata tm \
                 JOIN tracks t ON t.id = tm.track_id \
                 WHERE t.album_id = $1 AND tm.key = 'dr_album' AND tm.value <> '' \
                 LIMIT 1"
            }
        };
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(sql, &params)?
            .and_then(|cols| cols.first().and_then(|v| v.as_string()))
            .filter(|s| !s.trim().is_empty()))
    }

    pub fn bio_provenance(&self, id: i64) -> Result<Option<serde_json::Value>, TuneError> {
        let sql = match self.db.engine() {
            Engine::Sqlite => {
                "SELECT bio_source, bio_source_url, bio_license, bio_lang, bio_fetched_at \
                 FROM albums WHERE id = ?"
            }
            Engine::Postgres => {
                "SELECT bio_source, bio_source_url, bio_license, bio_lang, bio_fetched_at \
                 FROM albums WHERE id = $1"
            }
        };
        let params: [&dyn ToSqlValue; 1] = [&id];
        let row = self.db.query_one(sql, &params)?;
        Ok(row.and_then(|cols| {
            let source = cols
                .first()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty())?;
            Some(serde_json::json!({
                "source": source,
                "source_url": cols.get(1).and_then(|v| v.as_string()),
                "license": cols.get(2).and_then(|v| v.as_string()),
                "lang": cols.get(3).and_then(|v| v.as_string()),
                "fetched_at": cols.get(4).and_then(|v| v.as_string()),
            }))
        }))
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Album>, TuneError> {
        let fts_query = crate::db::engine::format_fts_query(self.db.engine(), query);
        let like = format!("%{query}%");
        let trimmed = query.trim();
        let sql = self.dialect_sql(sql::search, sql::search);
        let params: [&dyn ToSqlValue; 7] =
            [&fts_query, &like, &like, &like, &trimmed, &like, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_album).collect())
    }
}

fn row_to_album(cols: &Vec<SqlValue>) -> Album {
    Album {
        id: cols.first().and_then(|v| v.as_i64()),
        title: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        artist_id: cols.get(2).and_then(|v| v.as_i64()),
        artist_name: cols.get(3).and_then(|v| v.as_string()),
        year: cols.get(4).and_then(|v| v.as_i64()).map(|n| n as i32),
        original_year: cols.get(5).and_then(|v| v.as_i64()).map(|n| n as i32),
        genre: cols.get(6).and_then(|v| v.as_string()),
        // Index 23 (after the 23-col select): a.genres
        genres: cols.get(23).and_then(|v| v.as_string()),
        // Index 24: a.is_compilation (#1957). `as_i64` tolère l'entier des deux
        // moteurs ET le texte d'une base issue de `migrate-to-postgres` pas
        // encore soignée par la migration PG 028 — donc jamais de faux « non »
        // par simple désaccord de type. Absent/NUL = non.
        is_compilation: cols
            .get(24)
            .and_then(|v| v.as_i64())
            .map(|n| n != 0)
            .unwrap_or(false),
        // Index 25: added_at — absent de la plupart des requêtes (None) ;
        // le listing trié par date d'ajout le renseigne après coup, depuis
        // sa première passe (#1269).
        added_at: cols.get(25).and_then(|v| v.as_f64()),
        disc_count: cols.get(7).and_then(|v| v.as_i64()).map(|n| n as i32),
        track_count: cols.get(8).and_then(|v| v.as_i64()).map(|n| n as i32),
        cover_path: cols.get(9).and_then(|v| v.as_string()),
        source: cols
            .get(10)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "local".into()),
        source_id: cols.get(11).and_then(|v| v.as_string()),
        label: cols.get(12).and_then(|v| v.as_string()),
        catalog_number: cols.get(13).and_then(|v| v.as_string()),
        barcode: cols.get(14).and_then(|v| v.as_string()),
        format: cols.get(15).and_then(|v| v.as_string()),
        sample_rate: cols.get(16).and_then(|v| v.as_i64()).map(|n| n as i32),
        bit_depth: cols.get(17).and_then(|v| v.as_i64()).map(|n| n as i32),
        bio: cols.get(18).and_then(|v| v.as_string()),
        musicbrainz_release_id: cols.get(19).and_then(|v| v.as_string()),
        musicbrainz_release_group_id: cols.get(20).and_then(|v| v.as_string()),
        release_date: cols.get(21).and_then(|v| v.as_string()),
        original_date: cols.get(22).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    /// Pose une piste numérotée sur un album, comme le fait le scan.
    fn seed_track(db: &SqliteDb, album_id: i64, artist_id: i64, n: i32, path: &str) {
        seed_track_with_album_artist(db, album_id, artist_id, n, path, None);
    }

    fn seed_track_with_album_artist(
        db: &SqliteDb,
        album_id: i64,
        artist_id: i64,
        n: i32,
        path: &str,
        album_artist: Option<&str>,
    ) {
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let mut t = Track::new(format!("piste {n}"));
        t.album_id = Some(album_id);
        t.artist_id = Some(artist_id);
        t.album_artist = album_artist.map(str::to_string);
        t.track_number = n;
        t.file_path = Some(path.into());
        TrackRepo::new(db.clone()).create(&t).unwrap();
    }

    /// Crée un dossier d'album avec sa pochette, comme sur disque.
    ///
    /// `motif` désigne le disque ; `qualite` fait varier l'encodage d'un
    /// dossier à l'autre, comme le fait Qobuz. Deux dossiers d'un même disque
    /// n'ont donc pas un octet en commun — ce qui est exactement le cas que la
    /// comparaison par SHA-256 ratait (#1470).
    fn dossier_avec_pochette(
        base: &std::path::Path,
        artiste: &str,
        album: &str,
        motif: u32,
        qualite: u8,
    ) -> String {
        let d = base.join(artiste).join(album);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("cover.jpg"),
            crate::scanner::compilation::pochette_de_test(motif, 96, qualite),
        )
        .unwrap();
        d.to_string_lossy().into_owned()
    }

    /// #1440 — cas RÉEL : l'anthologie « OUF » rangée par artiste de piste.
    /// Même pochette dans chaque dossier ⇒ un seul album.
    #[test]
    fn a_scattered_anthology_lands_in_one_album() {
        const TITLE: &str = "OUF L'anthologie Souterraine 2015-2017";
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let artists = ArtistRepo::new(db.clone());
        let a1 = artists.create(&Artist::new("Corte Real".into())).unwrap();
        let a2 = artists.create(&Artist::new("Alligator".into())).unwrap();
        let f1 = dossier_avec_pochette(tmp.path(), "Corte Real", TITLE, 1, 92);
        let f2 = dossier_avec_pochette(tmp.path(), "Alligator", TITLE, 1, 55);

        let first = arepo
            .get_or_create_for_folder_with_track(&f1, TITLE, a1, None, None, Some(1))
            .unwrap();
        seed_track(&db, first.id.unwrap(), a1, 1, &format!("{f1}/01.flac"));

        let second = arepo
            .get_or_create_for_folder_with_track(&f2, TITLE, a2, None, None, Some(3))
            .unwrap();
        assert_eq!(second.id, first.id, "même disque, même pochette : un album");
    }

    /// #1440 — deux « Greatest Hits » distincts : pochettes différentes ET
    /// numéros qui se chevauchent. Aucune fusion.
    #[test]
    fn two_real_greatest_hits_stay_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let artists = ArtistRepo::new(db.clone());
        let a1 = artists.create(&Artist::new("Pat Benatar".into())).unwrap();
        let a2 = artists.create(&Artist::new("Police".into())).unwrap();
        let f1 = dossier_avec_pochette(tmp.path(), "Pat Benatar", "Greatest Hits", 1, 92);
        let f2 = dossier_avec_pochette(tmp.path(), "Police", "Greatest Hits", 3, 92);

        let first = arepo
            .get_or_create_for_folder_with_track(
                &f1,
                "Greatest Hits",
                a1,
                Some(2005),
                None,
                Some(1),
            )
            .unwrap();
        seed_track(&db, first.id.unwrap(), a1, 1, &format!("{f1}/01.flac"));

        let second = arepo
            .get_or_create_for_folder_with_track(
                &f2,
                "Greatest Hits",
                a2,
                Some(1992),
                None,
                Some(1),
            )
            .unwrap();
        assert_ne!(second.id, first.id);
    }

    /// LA RÉGRESSION livrée en #1442, reproduite : quatre volumes « ALLOPOP »
    /// éclatés par artiste, dont les pistes arrivent ENTRELACÉES.
    ///
    /// Le seul critère du numéro libre les avalait les uns après les autres —
    /// la piste 1 du volume 2 se présentait quand l'album ne contenait encore
    /// que les pistes 5 et 12 du volume 1. Résultat observé sur .18 : un album
    /// de 71 pistes avec chaque numéro en quatre exemplaires. La pochette du
    /// dossier est ce qui les sépare.
    #[test]
    fn four_interleaved_volumes_never_collapse() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let artists = ArtistRepo::new(db.clone());

        // (artiste, volume, numéro) dans un ordre volontairement entrelacé.
        let arrivees: [(&str, usize, i32); 8] = [
            ("Diane", 0, 5),
            ("Tristan", 1, 12),
            ("Gatien", 0, 12),
            ("Ma Fraisse", 2, 5),
            ("Loup", 1, 1),
            ("Nina", 2, 1),
            ("Oscar", 3, 5),
            ("Pia", 3, 1),
        ];
        let mut vus = std::collections::HashSet::new();
        for (artiste, vol, num) in arrivees {
            let f =
                dossier_avec_pochette(tmp.path(), artiste, "ALLOPOP", vol as u32, 60 + (num as u8));
            let aid = artists.create(&Artist::new(artiste.into())).unwrap();
            let album = arepo
                .get_or_create_for_folder_with_track(&f, "ALLOPOP", aid, None, None, Some(num))
                .unwrap();
            let id = album.id.unwrap();
            seed_track(&db, id, aid, num, &format!("{f}/{num:02}.flac"));
            vus.insert(id);
        }

        assert_eq!(
            vus.len(),
            4,
            "quatre pochettes ⇒ quatre albums, pas un seul (régression #1442)"
        );
    }

    #[test]
    fn crud_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let artist_id = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let mut album = Album::new("The Dark Side of the Moon".into());
        album.artist_id = Some(artist_id);
        album.year = Some(1973);

        let id = repo.create(&album).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "The Dark Side of the Moon");
        assert_eq!(fetched.artist_name.as_deref(), Some("Pink Floyd"));
        assert_eq!(fetched.year, Some(1973));

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn update_quality_backfills_genre_ignoring_empty_string_tracks() {
        // Regression for #3 (Fabien): `create_minimal` leaves albums.genre NULL,
        // and one of the album's tracks carries genre = '' (an empty tag frame).
        // The old backfill (`t.genre IS NOT NULL LIMIT 1` + `COALESCE(albums.genre,…)`)
        // could pick the empty track and pin albums.genre to '' — which the
        // completeness card counts as "without genre", making the genre card
        // read the whole catalogue as missing (identical to the cover card).
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db.clone());

        let artist_id = artist_repo
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();
        let album = repo
            .get_or_create("Kind of Blue", artist_id, Some(1959))
            .unwrap();
        let album_id = album.id.unwrap();
        // Album row created minimally: genre is NULL.
        assert_eq!(repo.get(album_id).unwrap().unwrap().genre, None);

        // Two tracks: one with an empty-string genre, one with a real genre.
        db.execute_batch(&format!(
            "INSERT INTO tracks (title, album_id, artist_id, genre) VALUES ('So What', {album_id}, {artist_id}, '');
             INSERT INTO tracks (title, album_id, artist_id, genre) VALUES ('Blue in Green', {album_id}, {artist_id}, 'Jazz');"
        ))
        .unwrap();

        repo.update_quality_from_tracks(album_id).unwrap();
        assert_eq!(
            repo.get(album_id).unwrap().unwrap().genre.as_deref(),
            Some("Jazz"),
            "backfill must skip the empty-string track and pick the real genre"
        );

        // Now poison the album with '' directly and confirm NULLIF heals it.
        db.execute_batch(&format!(
            "UPDATE albums SET genre = '' WHERE id = {album_id};"
        ))
        .unwrap();
        repo.update_quality_from_tracks(album_id).unwrap();
        assert_eq!(
            repo.get(album_id).unwrap().unwrap().genre.as_deref(),
            Some("Jazz"),
            "an already-empty album genre must be re-filled from a real track genre"
        );
    }

    #[test]
    fn get_or_create_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let artist_id = artist_repo.create(&Artist::new("Beatles".into())).unwrap();
        let a1 = repo
            .get_or_create("Abbey Road", artist_id, Some(1969))
            .unwrap();
        let a2 = repo
            .get_or_create("Abbey Road", artist_id, Some(1969))
            .unwrap();
        assert_eq!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn delete_orphans() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let _aid = artist_repo.create(&Artist::new("Test".into())).unwrap();
        repo.create(&Album::new("Orphan Album".into())).unwrap();
        let deleted = repo.delete_orphans().unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(repo.count().unwrap(), 0);
    }

    #[test]
    fn find_populated_sibling_resolves_duplicate_empty_album() {
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let arepo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());

        let aid = artist_repo
            .create(&Artist::new("Muddy Waters".into()))
            .unwrap();

        // Two album rows for the same title+artist: one populated, one empty —
        // the duplicate that makes "play album" 400 from the flat grids.
        let mut populated = Album::new("Folk Singer".into());
        populated.artist_id = Some(aid);
        let populated_id = arepo.create(&populated).unwrap();
        let mut empty = Album::new("Folk Singer".into());
        empty.artist_id = Some(aid);
        let empty_id = arepo.create(&empty).unwrap();

        let mut t = Track::new("My Home Is in the Delta".into());
        t.album_id = Some(populated_id);
        t.artist_id = Some(aid);
        t.file_path = Some("/blues/folk-singer/01.flac".into());
        trepo.create(&t).unwrap();

        // The empty duplicate resolves to the populated sibling.
        assert_eq!(
            arepo.find_populated_sibling(empty_id).unwrap(),
            Some(populated_id)
        );
        // The populated album has no *other* populated sibling → None.
        assert_eq!(arepo.find_populated_sibling(populated_id).unwrap(), None);
    }

    #[test]
    fn update_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Coltrane".into())).unwrap();
        let mut album = Album::new("A Love Supreme".into());
        album.artist_id = Some(aid);
        album.year = Some(1965);
        let id = repo.create(&album).unwrap();

        album.id = Some(id);
        album.genre = Some("Jazz".into());
        album.label = Some("Impulse!".into());
        album.bio = Some("A masterpiece".into());
        repo.update(&album).unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.genre.as_deref(), Some("Jazz"));
        assert_eq!(fetched.label.as_deref(), Some("Impulse!"));
        assert_eq!(fetched.bio.as_deref(), Some("A masterpiece"));
    }

    #[test]
    fn update_cover_path() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let id = repo.create(&Album::new("Test Album".into())).unwrap();
        repo.update_cover_path(id, "abc123").unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.cover_path.as_deref(), Some("abc123"));

        // COALESCE: does NOT overwrite existing cover_path
        repo.update_cover_path(id, "new_hash").unwrap();
        let fetched2 = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched2.cover_path.as_deref(), Some("abc123"));
    }

    #[test]
    fn force_update_cover_path() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let id = repo.create(&Album::new("Test Album".into())).unwrap();
        repo.update_cover_path(id, "abc123").unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.cover_path.as_deref(), Some("abc123"));

        // force: DOES overwrite existing cover_path (used by rescan endpoints)
        repo.force_update_cover_path(id, "new_hash").unwrap();
        let fetched2 = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched2.cover_path.as_deref(), Some("new_hash"));
    }

    #[test]
    fn list_albums() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Various".into())).unwrap();
        for title in ["Alpha", "Beta", "Gamma", "Delta", "Epsilon"] {
            let mut a = Album::new(title.into());
            a.artist_id = Some(aid);
            repo.create(&a).unwrap();
        }

        let all = repo.list(100, 0).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].title, "Alpha");
        assert_eq!(all[4].title, "Gamma");

        let page = repo.list(2, 2).unwrap();
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn list_recent_albums() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        for i in 0..5 {
            repo.create(&Album::new(format!("Album {i}"))).unwrap();
        }

        let recent = repo.list_recent(3).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].title, "Album 4");
    }

    #[test]
    fn list_by_artist() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid1 = artist_repo
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();
        let aid2 = artist_repo.create(&Artist::new("Coltrane".into())).unwrap();

        let mut a1 = Album::new("Kind of Blue".into());
        a1.artist_id = Some(aid1);
        a1.year = Some(1959);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Bitches Brew".into());
        a2.artist_id = Some(aid1);
        a2.year = Some(1970);
        repo.create(&a2).unwrap();

        let mut a3 = Album::new("A Love Supreme".into());
        a3.artist_id = Some(aid2);
        repo.create(&a3).unwrap();

        let miles_albums = repo.list_by_artist(aid1).unwrap();
        assert_eq!(miles_albums.len(), 2);
        assert_eq!(miles_albums[0].title, "Kind of Blue");
        assert_eq!(miles_albums[1].title, "Bitches Brew");
    }

    #[test]
    fn list_by_genre() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let mut a1 = Album::new("Jazz Album".into());
        a1.genre = Some("Jazz".into());
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Rock Album".into());
        a2.genre = Some("Rock".into());
        repo.create(&a2).unwrap();

        let mut a3 = Album::new("Fusion Album".into());
        a3.genres = Some(r#"["Jazz","Fusion"]"#.into());
        repo.create(&a3).unwrap();

        let mut a4 = Album::new("Jazz Blues Album".into());
        a4.genre = Some("Jazz; Blues".into());
        repo.create(&a4).unwrap();

        let mut a5 = Album::new("Blues Rock Album".into());
        a5.genre = Some("Blues/Rock".into());
        repo.create(&a5).unwrap();

        let jazz = repo.list_by_genre("Jazz").unwrap();
        assert_eq!(jazz.len(), 3);

        let blues = repo.list_by_genre("Blues").unwrap();
        assert_eq!(blues.len(), 2);

        let rock = repo.list_by_genre("Rock").unwrap();
        assert_eq!(rock.len(), 2);

        let mut a6 = Album::new("Prog Album".into());
        a6.genre = Some("Progressive Rock".into());
        repo.create(&a6).unwrap();
        let rock2 = repo.list_by_genre("Rock").unwrap();
        assert_eq!(rock2.len(), 2);

        let prog = repo.list_by_genre("Progressive Rock").unwrap();
        assert_eq!(prog.len(), 1);
    }

    #[test]
    fn list_without_cover() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();

        let mut a1 = Album::new("No Cover".into());
        a1.artist_id = Some(aid);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Has Cover".into());
        a2.artist_id = Some(aid);
        a2.cover_path = Some("hash123".into());
        repo.create(&a2).unwrap();

        let without = repo.list_without_cover().unwrap();
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].1, "No Cover");
    }

    #[test]
    fn search_albums() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let mut a = Album::new("The Dark Side of the Moon".into());
        a.artist_id = Some(aid);
        a.year = Some(1973);
        repo.create(&a).unwrap();

        let results = repo.search("dark", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "The Dark Side of the Moon");
    }

    /// #1391 — un album masqué sort des vues de découverte (grille, compteur
    /// de pagination, récents, artiste, genre, recherche), mais PAS de
    /// `get()` : masqué n'est pas supprimé, il doit rester jouable et
    /// démasquable. `?include_hidden=true` le fait réapparaître dans la
    /// grille — le retour rétro-compatible.
    #[test]
    fn un_album_masque_sort_des_vues_mais_reste_accessible() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let hidden = crate::db::hidden_repo::HiddenRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Kraftwerk".into()))
            .unwrap();
        let mut visible = Album::new("Autobahn".into());
        visible.artist_id = Some(aid);
        visible.genre = Some("Electro".into());
        repo.create(&visible).unwrap();
        let mut cache = Album::new("Trans-Europe Express".into());
        cache.artist_id = Some(aid);
        cache.genre = Some("Electro".into());
        let cache_id = repo.create(&cache).unwrap();

        assert!(hidden.hide_album(cache_id).unwrap());

        // Grille par défaut : exclu, et le total suit.
        let grille = repo
            .list_filtered(100, 0, "title", "asc", None, None, None, false, None)
            .unwrap();
        assert_eq!(
            grille.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(),
            vec!["Autobahn"]
        );
        assert_eq!(repo.count_visible().unwrap(), 1);
        assert_eq!(repo.count().unwrap(), 2, "le compte COMPLET reste entier");

        // `include_hidden` : le retour pour la vue de révision.
        let tout = repo
            .list_filtered(100, 0, "title", "asc", None, None, None, true, None)
            .unwrap();
        assert_eq!(tout.len(), 2);

        // Récents, artiste, genre, recherche : exclu partout.
        assert!(
            repo.list_recent(10)
                .unwrap()
                .iter()
                .all(|a| a.id != Some(cache_id))
        );
        assert!(
            repo.list_by_artist(aid)
                .unwrap()
                .iter()
                .all(|a| a.id != Some(cache_id))
        );
        assert!(
            repo.list_by_genre("Electro")
                .unwrap()
                .iter()
                .all(|a| a.id != Some(cache_id))
        );
        assert!(
            repo.search("Trans-Europe", 10).unwrap().is_empty(),
            "la recherche ne doit pas trahir l'album masqué"
        );

        // Masqué n'est pas supprimé : l'accès direct reste opérant.
        assert!(repo.get(cache_id).unwrap().is_some());

        // Réversible : tout revient.
        assert!(hidden.unhide_album(cache_id).unwrap());
        assert_eq!(repo.count_visible().unwrap(), 2);
        assert_eq!(repo.search("Trans-Europe", 10).unwrap().len(), 1);
    }

    #[test]
    fn get_by_musicbrainz_release_id() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let mut album = Album::new("Test".into());
        album.musicbrainz_release_id = Some("12345-abcde".into());
        let id = repo.create(&album).unwrap();

        let found = repo.get_by_musicbrainz_release_id("12345-abcde").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, Some(id));

        assert!(
            repo.get_by_musicbrainz_release_id("nonexistent")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn count_albums() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        repo.create(&Album::new("A".into())).unwrap();
        repo.create(&Album::new("B".into())).unwrap();
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn album_quality_classification() {
        let mut a = Album::new("DSD Album".into());
        a.format = Some("dsf".into());
        assert_eq!(a.quality(), Some("dsd".into()));

        let mut b = Album::new("Hi-Res Album".into());
        b.sample_rate = Some(96000);
        b.bit_depth = Some(24);
        b.format = Some("flac".into());
        assert_eq!(b.quality(), Some("hi-res".into()));

        let mut c = Album::new("CD Album".into());
        c.format = Some("flac".into());
        c.sample_rate = Some(44100);
        c.bit_depth = Some(16);
        assert_eq!(c.quality(), Some("cd".into()));

        let mut d = Album::new("Lossy Album".into());
        d.format = Some("mp3".into());
        assert_eq!(d.quality(), Some("lossy".into()));

        let e = Album::new("Unknown".into());
        assert_eq!(e.quality(), None);
    }

    #[test]
    fn album_to_json() {
        let mut a = Album::new("Test".into());
        a.format = Some("flac".into());
        a.sample_rate = Some(96000);
        a.bit_depth = Some(24);
        let json = a.to_json();
        assert_eq!(json["quality"], "hi-res");
        assert_eq!(json["title"], "Test");
    }

    #[test]
    fn get_or_create_without_year() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();
        let a1 = repo.get_or_create("Album", aid, None).unwrap();
        let a2 = repo.get_or_create("Album", aid, None).unwrap();
        assert_eq!(a1.id, a2.id);
    }

    #[test]
    fn get_or_create_same_title_different_artists() {
        // Regression test: "One by One" by Grey Reverend must NOT be merged
        // with "One by One" by Robert Francis.
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid1 = artist_repo
            .create(&Artist::new("Grey Reverend".into()))
            .unwrap();
        let aid2 = artist_repo
            .create(&Artist::new("Robert Francis".into()))
            .unwrap();

        let a1 = repo.get_or_create("One by One", aid1, Some(2010)).unwrap();
        let a2 = repo.get_or_create("One by One", aid2, Some(2013)).unwrap();

        // Must be two different albums
        assert_ne!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 2);

        // Same artist + same title => same album
        let a3 = repo.get_or_create("One by One", aid1, Some(2010)).unwrap();
        assert_eq!(a1.id, a3.id);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn get_or_create_with_mbid_same_title_different_artists() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid1 = artist_repo
            .create(&Artist::new("Grey Reverend".into()))
            .unwrap();
        let aid2 = artist_repo
            .create(&Artist::new("Robert Francis".into()))
            .unwrap();

        let a1 = repo
            .get_or_create_with_mbid("One by One", aid1, Some(2010), None)
            .unwrap();
        let a2 = repo
            .get_or_create_with_mbid("One by One", aid2, Some(2013), None)
            .unwrap();

        assert_ne!(a1.id, a2.id);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn get_or_create_with_mbid_distinct_releases_do_not_merge() {
        // Two editions with the SAME title+artist+year but DIFFERENT MusicBrainz
        // release ids must stay separate (Dominique) — while a later track of
        // the same album that is missing its MBID rejoins one of them instead of
        // creating a third phantom album.
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let aid = artist_repo
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();

        let a1 = repo
            .get_or_create_with_mbid("Kind of Blue", aid, Some(1959), Some("mbid-aaa"))
            .unwrap();
        let a2 = repo
            .get_or_create_with_mbid("Kind of Blue", aid, Some(1959), Some("mbid-bbb"))
            .unwrap();
        assert_ne!(a1.id, a2.id, "distinct MBIDs must not collapse");
        assert_eq!(repo.count().unwrap(), 2);

        // Same MBID again → returns the existing album, no new row.
        let a1_again = repo
            .get_or_create_with_mbid("Kind of Blue", aid, Some(1959), Some("mbid-aaa"))
            .unwrap();
        assert_eq!(a1_again.id, a1.id);

        // Untagged track (no MBID) matches by title+artist+year — must not spawn
        // a third album.
        let untagged = repo
            .get_or_create_with_mbid("Kind of Blue", aid, Some(1959), None)
            .unwrap();
        assert!(untagged.id == a1.id || untagged.id == a2.id);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn update_quality_from_tracks() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();
        let alid = album_repo
            .get_or_create("Test Album", aid, None)
            .unwrap()
            .id
            .unwrap();

        let mut t = crate::db::models::Track::new("Track 1".into());
        t.album_id = Some(alid);
        t.format = Some("flac".into());
        t.sample_rate = Some(96000);
        t.bit_depth = Some(24);
        t.file_path = Some("/test.flac".into());
        track_repo.create(&t).unwrap();

        album_repo.update_quality_from_tracks(alid).unwrap();
        let album = album_repo.get(alid).unwrap().unwrap();
        assert_eq!(album.format.as_deref(), Some("flac"));
        assert_eq!(album.sample_rate, Some(96000));
        assert_eq!(album.bit_depth, Some(24));
    }

    #[test]
    fn update_track_count() {
        let db = test_db();
        let album_repo = AlbumRepo::new(db.clone());
        let track_repo = crate::db::track_repo::TrackRepo::new(db);

        let alid = album_repo.create(&Album::new("Test".into())).unwrap();
        let mut t1 = crate::db::models::Track::new("A".into());
        t1.album_id = Some(alid);
        t1.file_path = Some("/a.flac".into());
        let mut t2 = crate::db::models::Track::new("B".into());
        t2.album_id = Some(alid);
        t2.file_path = Some("/b.flac".into());
        track_repo.create(&t1).unwrap();
        track_repo.create(&t2).unwrap();

        album_repo.update_track_count(alid).unwrap();
        let album = album_repo.get(alid).unwrap().unwrap();
        assert_eq!(album.track_count, Some(2));
    }

    #[test]
    fn unicode_album_title() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let mut a = Album::new("Concerto pour clarinette en la majeur".into());
        a.genre = Some("Classique".into());
        let id = repo.create(&a).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "Concerto pour clarinette en la majeur");
    }

    #[test]
    fn delete_nonexistent_album() {
        let db = test_db();
        let repo = AlbumRepo::new(db);
        repo.delete(999).unwrap();
    }

    #[test]
    fn list_sorted_by_title() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Various".into())).unwrap();
        for title in ["Gamma", "Alpha", "Beta"] {
            let mut a = Album::new(title.into());
            a.artist_id = Some(aid);
            repo.create(&a).unwrap();
        }

        let asc = repo.list_sorted(100, 0, "title", "asc").unwrap();
        assert_eq!(asc[0].title, "Alpha");
        assert_eq!(asc[2].title, "Gamma");

        let desc = repo.list_sorted(100, 0, "title", "desc").unwrap();
        assert_eq!(desc[0].title, "Gamma");
        assert_eq!(desc[2].title, "Alpha");
    }

    #[test]
    fn update_dates_backfills_null_year_without_overwriting() {
        // #1106: album.year is set at creation from the first track's year; if
        // that read errored (year missing) the album stayed NULL forever. A later
        // scan of a track that DOES carry the year must back-fill it — but must
        // never overwrite an album that already has a year.
        let db = test_db();
        let repo = AlbumRepo::new(db);

        // Album created without a year (as if the creating track's year was lost).
        let missing = repo.create(&Album::new("Missing".into())).unwrap();
        assert_eq!(repo.get(missing).unwrap().unwrap().year, None);
        repo.update_dates(missing, Some(1985), None, None, None)
            .unwrap();
        assert_eq!(
            repo.get(missing).unwrap().unwrap().year,
            Some(1985),
            "NULL year must be back-filled from a track that carries it"
        );
        // A second track reporting a different year must NOT overwrite it.
        repo.update_dates(missing, Some(1990), None, None, None)
            .unwrap();
        assert_eq!(
            repo.get(missing).unwrap().unwrap().year,
            Some(1985),
            "an existing year must be preserved (COALESCE, not overwrite)"
        );

        // An album that already has a year is left untouched.
        let mut has = Album::new("Has".into());
        has.year = Some(2001);
        let has = repo.create(&has).unwrap();
        repo.update_dates(has, Some(1999), None, None, None)
            .unwrap();
        assert_eq!(repo.get(has).unwrap().unwrap().year, Some(2001));
    }

    #[test]
    fn list_sorted_by_year() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid = artist_repo.create(&Artist::new("Artist".into())).unwrap();

        let mut a1 = Album::new("Old".into());
        a1.artist_id = Some(aid);
        a1.year = Some(1970);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("New".into());
        a2.artist_id = Some(aid);
        a2.year = Some(2020);
        repo.create(&a2).unwrap();

        let asc = repo.list_sorted(100, 0, "year", "asc").unwrap();
        assert_eq!(asc[0].title, "Old");
        assert_eq!(asc[1].title, "New");

        let desc = repo.list_sorted(100, 0, "release_date", "desc").unwrap();
        assert_eq!(desc[0].title, "New");
        assert_eq!(desc[1].title, "Old");
    }

    #[test]
    fn list_sorted_by_artist() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);

        let aid_z = artist_repo.create(&Artist::new("Zappa".into())).unwrap();
        let aid_a = artist_repo.create(&Artist::new("Abba".into())).unwrap();

        let mut a1 = Album::new("Hot Rats".into());
        a1.artist_id = Some(aid_z);
        repo.create(&a1).unwrap();

        let mut a2 = Album::new("Arrival".into());
        a2.artist_id = Some(aid_a);
        repo.create(&a2).unwrap();

        let asc = repo.list_sorted(100, 0, "artist", "asc").unwrap();
        assert_eq!(asc[0].title, "Arrival");
        assert_eq!(asc[1].title, "Hot Rats");

        let desc = repo.list_sorted(100, 0, "artist", "desc").unwrap();
        assert_eq!(desc[0].title, "Hot Rats");
    }

    /// Le drapeau « compilation » doit SURVIVRE à l'écriture (#1957). Il était
    /// lu dans les tags, utilisé au scan, puis jeté : aucune colonne, donc
    /// aucune requête capable de le rendre.
    #[test]
    fn le_drapeau_compilation_survit_a_une_relecture() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        let ordinaire = repo.create(&Album::new("Kind of Blue".into())).unwrap();
        let anthologie = repo.create(&Album::new("Jazz sur Seine".into())).unwrap();

        // Par défaut, personne n'est une compilation.
        assert!(
            !repo.get(anthologie).unwrap().unwrap().is_compilation,
            "un album neuf ne doit pas naître compilation"
        );

        repo.mark_compilation(anthologie).unwrap();

        assert!(
            repo.get(anthologie).unwrap().unwrap().is_compilation,
            "le drapeau posé par le scan doit se relire"
        );
        assert!(
            !repo.get(ordinaire).unwrap().unwrap().is_compilation,
            "marquer un album ne doit pas en marquer un autre"
        );

        // Le drapeau doit aussi ressortir des listes, pas seulement du get :
        // c'est la vue album qui le réclame.
        let liste = repo.list(100, 0).unwrap();
        let vue: Vec<(String, bool)> = liste
            .iter()
            .map(|a| (a.title.clone(), a.is_compilation))
            .collect();
        assert!(
            vue.contains(&("Jazz sur Seine".to_string(), true)),
            "la liste doit porter le drapeau : {vue:?}"
        );
        assert!(
            vue.contains(&("Kind of Blue".to_string(), false)),
            "la liste doit distinguer les deux : {vue:?}"
        );
    }

    /// `mark_compilation` LÈVE le drapeau et ne le baisse jamais — le scan voit
    /// les pistes une par une, et une anthologie dont la première piste, seule,
    /// ne ressemble à rien ne doit pas dépendre de l'ordre des fichiers.
    /// Rejouable sans effet : un rescan ne doit rien réécrire.
    #[test]
    fn marquer_une_compilation_est_idempotent() {
        let db = test_db();
        let repo = AlbumRepo::new(db);
        let id = repo.create(&Album::new("Anthologie".into())).unwrap();

        repo.mark_compilation(id).unwrap();
        repo.mark_compilation(id).unwrap();
        repo.mark_compilation(id).unwrap();

        assert!(repo.get(id).unwrap().unwrap().is_compilation);
    }

    /// `create` doit transporter le drapeau : sans ça, un album créé
    /// compilation ressortirait « non » et le round-trip du modèle mentirait.
    #[test]
    fn create_transporte_le_drapeau() {
        let db = test_db();
        let repo = AlbumRepo::new(db);
        let mut a = Album::new("Now That's What I Call Music".into());
        a.is_compilation = true;
        let id = repo.create(&a).unwrap();
        assert!(repo.get(id).unwrap().unwrap().is_compilation);
    }

    /// Filtrer la bibliothèque sur les compilations — un des trois usages que
    /// l'absence de colonne interdisait (#1957).
    #[test]
    fn list_filtered_isole_les_compilations() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        repo.create(&Album::new("Kind of Blue".into())).unwrap();
        let comp = repo.create(&Album::new("Jazz sur Seine".into())).unwrap();
        repo.mark_compilation(comp).unwrap();

        let titres = |v: Vec<Album>| -> Vec<String> { v.into_iter().map(|a| a.title).collect() };

        let seulement = titres(
            repo.list_filtered(100, 0, "title", "asc", None, None, Some(true), true, None)
                .unwrap(),
        );
        assert_eq!(seulement, vec!["Jazz sur Seine".to_string()]);

        let sauf = titres(
            repo.list_filtered(100, 0, "title", "asc", None, None, Some(false), true, None)
                .unwrap(),
        );
        assert_eq!(sauf, vec!["Kind of Blue".to_string()]);

        let tout = titres(
            repo.list_filtered(100, 0, "title", "asc", None, None, None, true, None)
                .unwrap(),
        );
        assert_eq!(tout.len(), 2, "sans filtre, les deux albums : {tout:?}");
    }

    /// `added_at` est injecté JUSTE AVANT `FROM albums a`, donc l'ajout de
    /// `is_compilation` en fin de `select_album` l'a décalé d'un index.
    /// `row_to_album` lit par index : ce test épingle les DEUX colonnes
    /// ensemble, pour qu'un futur ajout ne fasse pas taire la date d'ajout.
    #[test]
    fn is_compilation_et_added_at_cohabitent_dans_le_meme_select() {
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let db = test_db();
        let repo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());

        let id = repo.create(&Album::new("Anthologie".into())).unwrap();
        repo.mark_compilation(id).unwrap();
        let mut t = Track::new("piste".into());
        t.album_id = Some(id);
        t.file_path = Some("/a.flac".into());
        t.file_mtime = Some(1000.0);
        trepo.create(&t).unwrap();

        let tries = repo.list_sorted(100, 0, "added_at", "desc").unwrap();
        let a = tries.first().expect("un album attendu");
        assert!(
            a.is_compilation,
            "le drapeau doit survivre au SELECT enrichi de added_at"
        );
        assert!(
            a.added_at.is_some(),
            "added_at doit rester lu, malgré le décalage d'index"
        );
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::get_by_id(&s).ends_with("WHERE a.id = ?"));
        assert!(sql::get_by_id(&p).ends_with("WHERE a.id = $1"));
        assert!(sql::get_by_title(&p).contains("LOWER(a.title) = LOWER($1)"));
        assert!(sql::create_minimal(&p).contains("VALUES ($1, $2, $3)"));
        assert!(!sql::list_by_artist(&p).contains("COLLATE"));
    }

    #[test]
    fn search_uses_engine_specific_fts_clause() {
        let s_sql = sql::search(&SqliteDialect);
        assert!(s_sql.contains("a.id IN (SELECT rowid FROM albums_fts WHERE albums_fts MATCH ?)"));
        let p_sql = sql::search(&PostgresDialect);
        assert!(p_sql.contains("a.search_tsv @@ to_tsquery('simple', unaccent($1))"));
    }

    #[test]
    fn list_sorted_by_added_at() {
        let db = test_db();
        let repo = AlbumRepo::new(db);

        repo.create(&Album::new("First".into())).unwrap();
        repo.create(&Album::new("Second".into())).unwrap();
        repo.create(&Album::new("Third".into())).unwrap();

        let asc = repo.list_sorted(100, 0, "added_at", "asc").unwrap();
        assert_eq!(asc[0].title, "First");
        assert_eq!(asc[2].title, "Third");

        let desc = repo.list_sorted(100, 0, "added_at", "desc").unwrap();
        assert_eq!(desc[0].title, "Third");
        assert_eq!(desc[2].title, "First");
    }

    #[test]
    fn dynamic_range_reads_the_tag_from_any_track_of_the_album() {
        use crate::db::models::Track;
        use crate::db::track_metadata_repo::TrackMetadataRepo;
        use crate::db::track_repo::TrackRepo;

        let db = test_db();
        // `track_metadata` arrives by migration, not by CORE_SCHEMA, so the
        // in-memory fixture does not have it. Create it here rather than run the
        // whole migration chain: this test is about the join, not the schema.
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS track_metadata (
                 track_id INTEGER NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 PRIMARY KEY (track_id, key)
             );",
        )
        .unwrap();
        let arepo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());
        let mrepo = TrackMetadataRepo::new(db.clone());

        let album_id = arepo.create(&Album::new("Tri Repetae".into())).unwrap();
        let mut t1 = Track::new("Dael".into());
        t1.album_id = Some(album_id);
        t1.file_path = Some("/m/1.flac".into());
        let id1 = trepo.create(&t1).unwrap();
        let mut t2 = Track::new("Clipper".into());
        t2.album_id = Some(album_id);
        t2.file_path = Some("/m/2.flac".into());
        let id2 = trepo.create(&t2).unwrap();

        // No tag anywhere yet: nothing to show, and that is the common case.
        assert_eq!(arepo.dynamic_range(album_id).unwrap(), None);

        // The tag lives in each file; the scanner therefore writes it per track
        // even though it describes the album. Tagging the SECOND track proves
        // the lookup does not just read the first one.
        mrepo.set(id2, "dr_album", "12").unwrap();
        assert_eq!(
            arepo.dynamic_range(album_id).unwrap().as_deref(),
            Some("12")
        );

        // An empty value must not masquerade as a measurement.
        mrepo.set(id1, "dr_album", "").unwrap();
        mrepo.delete(id2, "dr_album").unwrap();
        assert_eq!(arepo.dynamic_range(album_id).unwrap(), None);

        // DR0 is a real measurement on a crushed master, not an absence.
        mrepo.set(id1, "dr_album", "0").unwrap();
        assert_eq!(arepo.dynamic_range(album_id).unwrap().as_deref(), Some("0"));
    }

    // ------------------------------------------------------------------
    // #2144 — classer et filtrer les albums par tranches de Dynamic Range.
    // ------------------------------------------------------------------

    /// `test_db()` monte `CORE_SCHEMA` seul ; `track_metadata` arrive par
    /// migration. Les tests de DR la créent donc à la main, comme
    /// `dynamic_range_reads_the_tag_from_any_track_of_the_album`.
    fn db_avec_track_metadata() -> SqliteDb {
        let db = test_db();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS track_metadata (
                 track_id INTEGER NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 PRIMARY KEY (track_id, key)
             );",
        )
        .unwrap();
        db
    }

    /// Un album d'une piste, avec ou sans tag `dr_album`. La valeur est passée
    /// TELLE QUELLE : c'est ainsi que le scan l'écrit, `normalise_dr` ne
    /// garantissant rien de plus qu'un préfixe retiré.
    fn album_avec_dr(db: &SqliteDb, titre: &str, dr: Option<&str>) -> i64 {
        use crate::db::models::Track;
        use crate::db::track_metadata_repo::TrackMetadataRepo;
        use crate::db::track_repo::TrackRepo;

        let album_id = AlbumRepo::new(db.clone())
            .create(&Album::new(titre.to_string()))
            .unwrap();
        let mut t = Track::new(format!("{titre} — piste"));
        t.album_id = Some(album_id);
        t.file_path = Some(format!("/m/{titre}.flac"));
        let tid = TrackRepo::new(db.clone()).create(&t).unwrap();
        if let Some(v) = dr {
            TrackMetadataRepo::new(db.clone())
                .set(tid, "dr_album", v)
                .unwrap();
        }
        album_id
    }

    fn titres(albums: &[Album]) -> Vec<&str> {
        albums.iter().map(|a| a.title.as_str()).collect()
    }

    /// La bibliothèque d'essai des trois tests suivants : trois albums tagués,
    /// un sans tag, un dont le tag n'est pas un nombre.
    fn bibliotheque_dr() -> SqliteDb {
        let db = db_avec_track_metadata();
        album_avec_dr(&db, "Alpha", Some("6"));
        album_avec_dr(&db, "Bravo", Some("14"));
        album_avec_dr(&db, "Charlie", Some("9"));
        album_avec_dr(&db, "Delta", None);
        // `normalise_dr` recopie tel quel ce qui n'est pas une suite de
        // chiffres : « DR12.5 » ressort « DR12.5 ». Sans garde, le CAST en
        // ferait un 0 en SQLite et ferait ÉCHOUER la requête en PostgreSQL.
        album_avec_dr(&db, "Echo", Some("DR12.5"));
        db
    }

    /// #2144 — le tri numérique, et la place des albums non tagués.
    ///
    /// Le tag est stocké en TEXT : sans CAST, « 14 » se rangerait AVANT « 6 »
    /// (comparaison de chaînes), ce que le commit 7cdc93ff annonçait déjà comme
    /// le piège à traiter. Et un album sans tag n'a pas un DR bas : il n'en a
    /// pas — il termine la liste dans LES DEUX sens.
    #[test]
    fn le_tri_par_dynamic_range_est_numerique_et_relegue_les_non_tagues_2144() {
        let db = bibliotheque_dr();
        let repo = AlbumRepo::new(db);

        let asc = repo
            .list_filtered(100, 0, "dynamic_range", "asc", None, None, None, true, None)
            .unwrap();
        assert_eq!(
            titres(&asc),
            vec!["Alpha", "Charlie", "Bravo", "Delta", "Echo"],
            "6 < 9 < 14 (et non « 14 » < « 6 » comme le ferait une comparaison \
             de chaînes), puis les sans-valeur par titre"
        );

        let desc = repo
            .list_filtered(
                100,
                0,
                "dynamic_range",
                "desc",
                None,
                None,
                None,
                true,
                None,
            )
            .unwrap();
        assert_eq!(
            titres(&desc),
            vec!["Bravo", "Charlie", "Alpha", "Delta", "Echo"],
            "en DÉCROISSANT aussi les non tagués finissent — les remonter en \
             tête reviendrait à leur prêter le DR le plus élevé"
        );

        // L'alias court, celui qu'un client abrégerait naturellement.
        assert_eq!(
            titres(
                &repo
                    .list_filtered(100, 0, "dr", "asc", None, None, None, true, None)
                    .unwrap()
            ),
            vec!["Alpha", "Charlie", "Bravo", "Delta", "Echo"]
        );
    }

    /// #2144 — la tranche. Bornes incluses, chacune facultative, et le
    /// non-tagué n'y entre JAMAIS.
    #[test]
    fn la_tranche_de_dynamic_range_est_inclusive_et_exclut_les_non_tagues_2144() {
        let db = bibliotheque_dr();
        album_avec_dr(&db, "Zoulou", Some("0"));
        let repo = AlbumRepo::new(db);

        let tranche = |min, max| {
            let r = DrRange::new(min, max).unwrap();
            let page = repo
                .list_filtered(
                    100,
                    0,
                    "dynamic_range",
                    "asc",
                    None,
                    None,
                    None,
                    true,
                    Some(r),
                )
                .unwrap();
            // Le compteur de pagination doit compter EXACTEMENT la même chose
            // que la liste, sinon la grille saute des pages (#1391, #1269).
            assert_eq!(
                repo.count_in_dr_range(r, true).unwrap(),
                page.len() as i64,
                "le total annoncé ne compte pas la même tranche que la liste"
            );
            titres(&page)
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(tranche(Some(8), Some(14)), vec!["Charlie", "Bravo"]);
        assert_eq!(
            tranche(Some(10), None),
            vec!["Bravo"],
            "tranche ouverte en haut"
        );
        assert_eq!(
            tranche(None, Some(8)),
            vec!["Zoulou", "Alpha"],
            "tranche ouverte en bas — et DR0 est une vraie mesure, celle d'un \
             master saturé, pas une absence"
        );
        assert_eq!(
            tranche(Some(9), Some(9)),
            vec!["Charlie"],
            "bornes égales = une valeur unique, bornes INCLUSES"
        );
        assert!(
            tranche(Some(20), Some(30)).is_empty(),
            "une tranche vide rend zéro album, jamais la bibliothèque entière"
        );
        // Delta (sans tag) et Echo (tag non numérique) ne sortent d'AUCUNE
        // tranche, si large soit-elle.
        assert_eq!(
            tranche(Some(0), Some(99)),
            vec!["Zoulou", "Alpha", "Charlie", "Bravo"],
            "seuls les albums porteurs d'un DR numérique entrent dans une tranche"
        );

        // Aucune borne = AUCUN filtre (piège n°1 de `facet_filter`), et non un
        // filtre qui ne rendrait rien.
        assert_eq!(DrRange::new(None, None), None);
    }

    /// #2144 — rétro-compatibilité : sans paramètre de DR, la requête est
    /// EXACTEMENT celle d'avant.
    ///
    /// La preuve est structurelle et non déclarative : cette base n'a même pas
    /// de table `track_metadata`. Si le listage par défaut posait la jointure
    /// DR, il échouerait sur « no such table » ; il rend la bibliothèque.
    #[test]
    fn sans_parametre_de_dr_aucune_jointure_n_est_ajoutee_2144() {
        let db = test_db();
        let repo = AlbumRepo::new(db.clone());
        repo.create(&Album::new("Amnesiac".into())).unwrap();
        repo.create(&Album::new("Kid A".into())).unwrap();

        for sort in ["title", "added_at", "year", "artist", "id"] {
            let page = repo
                .list_filtered(100, 0, sort, "asc", None, None, None, true, None)
                .unwrap();
            assert_eq!(page.len(), 2, "tri {sort} : la liste doit rester servie");
        }
    }

    /// #2144 — les valeurs offertes aux tranches sont MESURÉES, pas inventées.
    #[test]
    fn les_valeurs_de_dynamic_range_disponibles_sont_celles_de_la_base_2144() {
        let db = db_avec_track_metadata();
        assert!(
            AlbumRepo::new(db.clone())
                .dynamic_range_values()
                .unwrap()
                .is_empty(),
            "bibliothèque sans tag : aucune facette à proposer"
        );

        album_avec_dr(&db, "Alpha", Some("14"));
        album_avec_dr(&db, "Bravo", Some("6"));
        album_avec_dr(&db, "Charlie", Some("14"));
        album_avec_dr(&db, "Delta", None);
        album_avec_dr(&db, "Echo", Some("DR12.5"));
        album_avec_dr(&db, "Foxtrot", Some("0"));
        // Que des chiffres, et pourtant hors de portée d'un `INTEGER`
        // PostgreSQL : un seul tag corrompu de cette forme ferait échouer la
        // requête entière — donc remonterait une grille VIDE, pas une valeur
        // fausse.
        album_avec_dr(&db, "Golf", Some("99999999999999999999"));

        assert_eq!(
            AlbumRepo::new(db).dynamic_range_values().unwrap(),
            vec![0, 6, 14],
            "valeurs distinctes, croissantes ; le tag non numérique et le tag \
             démesuré sont écartés, DR0 conservé"
        );
    }

    /// #2144 — un album masqué (#1391) ne revient pas par la porte du DR :
    /// ni dans la liste, ni dans le compteur, ni dans les facettes.
    #[test]
    fn le_filtre_dr_respecte_le_masquage_2144() {
        let db = db_avec_track_metadata();
        album_avec_dr(&db, "Visible", Some("12"));
        let cache = album_avec_dr(&db, "Caché", Some("13"));
        crate::db::hidden_repo::HiddenRepo::new(db.clone())
            .hide_album(cache)
            .unwrap();
        let repo = AlbumRepo::new(db);
        let r = DrRange::new(Some(10), Some(20)).unwrap();

        let page = repo
            .list_filtered(
                100,
                0,
                "dynamic_range",
                "asc",
                None,
                None,
                None,
                false,
                Some(r),
            )
            .unwrap();
        assert_eq!(titres(&page), vec!["Visible"]);
        assert_eq!(repo.count_in_dr_range(r, false).unwrap(), 1);
        assert_eq!(
            repo.count_in_dr_range(r, true).unwrap(),
            2,
            "`include_hidden` rend le masqué à la vue de révision"
        );
        assert_eq!(repo.dynamic_range_values().unwrap(), vec![12]);
    }

    /// #2144 — le SQL doit être VALIDE SUR LES DEUX MOTEURS, et le rester.
    ///
    /// Deux pièges, chacun mortel sur un seul des deux :
    /// * `GLOB` n'existe pas en PostgreSQL, `~` n'existe pas en SQLite ;
    /// * `CAST('DR12.5' AS INTEGER)` vaut 0 en SQLite (faux silencieux) et
    ///   ÉCHOUE en PostgreSQL (`invalid input syntax`) — la grille entière
    ///   remonterait vide, comme en #1269 sur `.15`.
    ///
    /// Et la forme : une jointure GROUPÉE, jamais une sous-requête corrélée
    /// sur `a.id`, qui serait ré-évaluée par ligne triée.
    #[test]
    fn la_jointure_dr_parle_les_deux_dialectes_et_reste_groupee_2144() {
        let sqlite = AlbumRepo::dr_album_join(Engine::Sqlite);
        let pg = AlbumRepo::dr_album_join(Engine::Postgres);

        assert!(sqlite.contains("tm.value NOT GLOB '*[^0-9]*'"), "{sqlite}");
        assert!(!sqlite.contains('~'), "`~` n'est pas du SQLite : {sqlite}");
        assert!(pg.contains("tm.value ~ '^[0-9]+$'"), "{pg}");
        assert!(
            !pg.contains("GLOB"),
            "`GLOB` n'est pas du PostgreSQL : {pg}"
        );

        for sql in [&sqlite, &pg] {
            assert!(sql.contains("GROUP BY t.album_id"), "{sql}");
            assert!(sql.contains("CAST(tm.value AS INTEGER)"), "{sql}");
            assert!(
                sql.contains("LENGTH(tm.value) <= 3"),
                "sans borne de longueur, un tag « que des chiffres » mais \
                 démesuré déborde l'INTEGER de PostgreSQL : {sql}"
            );
            let derivee = sql.split(") dr ON").next().unwrap();
            assert!(
                !derivee.contains("a."),
                "la table dérivée ne doit RIEN corréler à l'album courant, \
                 sinon elle est ré-évaluée par ligne (#1269) : {derivee}"
            );
        }
    }

    #[test]
    fn client_sort_key_aliases_match_canonical() {
        // The web client's sort dropdown sends "added_date" and "original_year"
        // (LibraryView AlbumSortKey), but the SQL layer's canonical keys are
        // "added_at" and "year". Before aliasing, the unknown keys fell through to
        // the `a.id` default — so "sort by date added" only *looked* right for the
        // most-recently-added albums (their ids are the highest), the "only the
        // first few albums are sorted" report (Bilou, #1102). The aliases must
        // route to the real logic, not the id fallback.
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());

        // ids A<B<C, but first_seen makes A newest and C oldest — the OPPOSITE of
        // id order, so the id fallback (DESC → C,B,A) is distinguishable from the
        // real "date added" order (DESC → A,B,C).
        let a = arepo.create(&Album::new("A".into())).unwrap();
        let b = arepo.create(&Album::new("B".into())).unwrap();
        let c = arepo.create(&Album::new("C".into())).unwrap();
        for (album_id, path, mtime) in [
            (a, "/a.flac", 1.0),
            (b, "/b.flac", 2.0),
            (c, "/c.flac", 3.0),
        ] {
            let mut t = Track::new("t".into());
            t.album_id = Some(album_id);
            t.file_path = Some(path.into());
            t.file_mtime = Some(mtime);
            trepo.create(&t).unwrap();
        }
        for (path, seen) in [
            ("/a.flac", 3000.0f64),
            ("/b.flac", 2000.0),
            ("/c.flac", 1000.0),
        ] {
            db.execute_batch(&format!(
                "UPDATE file_first_seen SET first_seen_at = {seen} WHERE file_path = '{path}';"
            ))
            .unwrap();
        }

        let titles = |v: Vec<Album>| v.into_iter().map(|a| a.title).collect::<Vec<_>>();
        let canon = titles(arepo.list_sorted(100, 0, "added_at", "desc").unwrap());
        let alias = titles(arepo.list_sorted(100, 0, "added_date", "desc").unwrap());
        assert_eq!(alias, canon, "added_date must alias added_at");
        assert_eq!(
            alias,
            vec!["A", "B", "C"],
            "must follow first_seen, not the id fallback"
        );

        // "original_year" must behave like "year" (New 2020 before Old 1970,
        // NULL-year albums last), not fall through to id order.
        let mut old = Album::new("Old".into());
        old.year = Some(1970);
        arepo.create(&old).unwrap();
        let mut new = Album::new("New".into());
        new.year = Some(2020);
        arepo.create(&new).unwrap();
        let by_year = titles(arepo.list_sorted(100, 0, "year", "desc").unwrap());
        let by_orig = titles(arepo.list_sorted(100, 0, "original_year", "desc").unwrap());
        assert_eq!(by_orig, by_year, "original_year must alias year");
        assert_eq!(by_orig[0], "New");
        assert_eq!(by_orig[1], "Old");
    }

    #[test]
    fn added_at_sorts_by_persistent_first_seen_not_id_or_mtime() {
        // Regression (eric): a full rescan does DELETE FROM albums + reinsert,
        // so album ids reflect filesystem-walk order, not add order; file mtime
        // is also unreliable for bulk-copied NAS libraries. "Date added" sorts
        // by the persistent `file_first_seen` timestamp (survives full rescan).
        // Here ids are A<B<C and mtimes make C newest, but first_seen makes A
        // newest — so the sort must follow first_seen, overriding both.
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());

        let a = arepo.create(&Album::new("A".into())).unwrap();
        let b = arepo.create(&Album::new("B".into())).unwrap();
        let c = arepo.create(&Album::new("C".into())).unwrap();

        // mtimes deliberately opposite to the desired order (C newest by mtime).
        for (album_id, path, mtime) in [
            (a, "/a.flac", 1000.0),
            (b, "/b.flac", 2000.0),
            (c, "/c.flac", 3000.0),
        ] {
            let mut t = Track::new("t".into());
            t.album_id = Some(album_id);
            t.file_path = Some(path.into());
            t.file_mtime = Some(mtime);
            trepo.create(&t).unwrap();
        }

        // Persistent first-seen makes A newest, C oldest (opposite of mtime/id).
        for (path, seen) in [
            ("/a.flac", 3000.0f64),
            ("/b.flac", 2000.0),
            ("/c.flac", 1000.0),
        ] {
            db.execute_batch(&format!(
                "UPDATE file_first_seen SET first_seen_at = {seen} WHERE file_path = '{path}';"
            ))
            .unwrap();
        }

        // desc = most recently added first → A (3000), B (2000), C (1000)
        let desc = arepo.list_sorted(100, 0, "added_at", "desc").unwrap();
        assert_eq!(desc[0].title, "A");
        assert_eq!(desc[1].title, "B");
        assert_eq!(desc[2].title, "C");

        // asc = oldest first → C, B, A
        let asc = arepo.list_sorted(100, 0, "added_at", "asc").unwrap();
        assert_eq!(asc[0].title, "C");
        assert_eq!(asc[2].title, "A");
    }

    #[test]
    fn added_at_falls_back_to_mtime_without_first_seen() {
        // When a track has no file_first_seen row yet (e.g. legacy row inserted
        // outside create()), the sort falls back to file mtime via COALESCE.
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());

        let a = arepo.create(&Album::new("A".into())).unwrap();
        let b = arepo.create(&Album::new("B".into())).unwrap();

        for (album_id, path, mtime) in [(a, "/a.flac", 3000.0), (b, "/b.flac", 1000.0)] {
            let mut t = Track::new("t".into());
            t.album_id = Some(album_id);
            t.file_path = Some(path.into());
            t.file_mtime = Some(mtime);
            trepo.create(&t).unwrap();
        }

        // Remove the auto-recorded first_seen rows so the fallback path is used.
        db.execute_batch("DELETE FROM file_first_seen;").unwrap();

        let desc = arepo.list_sorted(100, 0, "added_at", "desc").unwrap();
        assert_eq!(desc[0].title, "A"); // mtime 3000 newest
        assert_eq!(desc[1].title, "B");
    }

    #[test]
    fn added_at_mtime_expression_is_column_type_agnostic() {
        // On Postgres, tracks.file_mtime is TEXT on some installs and DOUBLE
        // PRECISION on others (schema drift between install vintages — .15 is
        // DOUBLE). The sort expression must be valid for both: it casts the
        // column to TEXT before the NULLIF/CAST-to-double dance. SQLite's
        // soft affinities let one column hold both representations, so store
        // a numeric mtime, a text mtime and an empty-string mtime and check
        // the sort still works and orders by the numeric value.
        use crate::db::models::Track;
        use crate::db::track_repo::TrackRepo;
        let db = test_db();
        let arepo = AlbumRepo::new(db.clone());
        let trepo = TrackRepo::new(db.clone());

        let a = arepo.create(&Album::new("A".into())).unwrap();
        let b = arepo.create(&Album::new("B".into())).unwrap();
        let c = arepo.create(&Album::new("C".into())).unwrap();
        for (album_id, path) in [(a, "/a.flac"), (b, "/b.flac"), (c, "/c.flac")] {
            let mut t = Track::new("t".into());
            t.album_id = Some(album_id);
            t.file_path = Some(path.into());
            trepo.create(&t).unwrap();
        }
        // Force the three storage classes: REAL (double installs), TEXT
        // (text installs), and the empty string NULLIF must neutralise.
        db.execute_batch(
            "UPDATE tracks SET file_mtime = 3000.0 WHERE file_path = '/a.flac';
             UPDATE tracks SET file_mtime = '1000' WHERE file_path = '/b.flac';
             UPDATE tracks SET file_mtime = '' WHERE file_path = '/c.flac';
             DELETE FROM file_first_seen;",
        )
        .unwrap();

        let desc = arepo.list_sorted(100, 0, "added_at", "desc").unwrap();
        let titles: Vec<_> = desc.into_iter().map(|al| al.title).collect();
        // A (3000) before B (1000); C has no usable timestamp → NULLS LAST.
        assert_eq!(titles, vec!["A", "B", "C"]);
    }

    #[test]
    fn with_backend_constructor_full() {
        // All methods now go through DbBackend — no more sqlite_legacy.
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let aid = artist_repo.create(&Artist::new("X".into())).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = AlbumRepo::with_backend(backend);
        let id = repo.create(&Album::new("Album X".into())).unwrap();
        assert!(repo.get(id).unwrap().is_some());
        // Previously-legacy methods now work via DbBackend.
        let a = repo.get_or_create("Created", aid, Some(2024)).unwrap();
        assert!(a.id.is_some());
        // Idempotent — second call returns the same row.
        let a2 = repo.get_or_create("Created", aid, Some(2024)).unwrap();
        assert_eq!(a.id, a2.id);
        // list_by_genre returns an empty list rather than erroring.
        assert!(repo.list_by_genre("Jazz").unwrap().is_empty());
    }

    /// One folder is one album, whatever the tracks' sample rates.
    ///
    /// The case that motivated this: a box set whose discs are 24/192, 16/44.1
    /// and 24/48 used to become three albums titled "X", "X (192kHz/24bit)" and
    /// "X (48kHz/24bit)" because the quality tier was appended to the title.
    #[test]
    fn one_folder_is_one_album_across_quality_tiers() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let aid = artist_repo
            .create(&Artist::new("Green Day".into()))
            .unwrap();
        let folder = "/music/Green Day/American Idiot";

        let first = repo
            .get_or_create_for_folder(folder, "American Idiot", aid, Some(2004), None)
            .unwrap();
        let second = repo
            .get_or_create_for_folder(folder, "American Idiot", aid, Some(2004), None)
            .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(
            repo.folder_path_of(first.id.unwrap()).unwrap().as_deref(),
            Some(folder)
        );
    }

    /// Two folders are two albums, even sharing title, artist and year — this is
    /// what the `quality_split` setting promises ("if the same album exists in CD
    /// and Hi-Res, create two separate entries"), now without a suffix in the
    /// title: the client renders the quality from `sample_rate`/`bit_depth`.
    #[test]
    fn two_folders_stay_two_albums() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let aid = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();

        let cd = repo
            .get_or_create_for_folder(
                "/music/PF/Division Bell",
                "The Division Bell",
                aid,
                Some(1994),
                None,
            )
            .unwrap();
        let hires = repo
            .get_or_create_for_folder(
                "/music/PF/Division Bell (24-192)",
                "The Division Bell",
                aid,
                Some(1994),
                None,
            )
            .unwrap();

        assert_ne!(cd.id, hires.id, "two rips must not be merged");
        assert_eq!(cd.title, hires.title, "and neither title carries a suffix");
    }

    /// An album indexed before folders were recorded adopts the first folder that
    /// claims it, so a library converts as it is rescanned instead of doubling.
    #[test]
    fn a_folderless_album_is_adopted_not_duplicated() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let aid = artist_repo.create(&Artist::new("The Who".into())).unwrap();

        // As the old scanner would have left it: no folder recorded.
        let legacy = repo.get_or_create("Tommy", aid, Some(1969)).unwrap();
        assert!(repo.folder_path_of(legacy.id.unwrap()).unwrap().is_none());

        let rescanned = repo
            .get_or_create_for_folder("/music/The Who/Tommy", "Tommy", aid, Some(1969), None)
            .unwrap();

        assert_eq!(legacy.id, rescanned.id, "the existing row must be reused");
        assert_eq!(
            repo.folder_path_of(legacy.id.unwrap()).unwrap().as_deref(),
            Some("/music/The Who/Tommy")
        );
    }

    /// With no folder to go on, identity falls back to exactly what it was.
    #[test]
    fn an_empty_folder_falls_back_to_title_and_artist() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let aid = artist_repo.create(&Artist::new("Nobody".into())).unwrap();

        let a = repo
            .get_or_create_for_folder("", "Untitled", aid, None, None)
            .unwrap();
        let b = repo
            .get_or_create_for_folder("", "Untitled", aid, None, None)
            .unwrap();
        assert_eq!(a.id, b.id);
        assert!(repo.folder_path_of(a.id.unwrap()).unwrap().is_none());
    }

    /// The MusicBrainz rule still wins where it applies: two distinct releases
    /// in two folders stay distinct, and the same release id is not duplicated.
    #[test]
    fn musicbrainz_identity_survives_folder_identity() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let aid = artist_repo.create(&Artist::new("Artist".into())).unwrap();

        let one = repo
            .get_or_create_for_folder("/m/A", "Album", aid, Some(2000), Some("mbid-1"))
            .unwrap();
        let two = repo
            .get_or_create_for_folder("/m/B", "Album", aid, Some(2000), Some("mbid-2"))
            .unwrap();
        assert_ne!(one.id, two.id);

        // Same folder, same release id → same row.
        let again = repo
            .get_or_create_for_folder("/m/A", "Album", aid, Some(2000), Some("mbid-1"))
            .unwrap();
        assert_eq!(one.id, again.id);
    }

    /// Reproduction du bug .15 : le watcher voit le premier fichier d'un
    /// dossier pendant son écriture (tags artiste illisibles) → l'album est
    /// créé sous « Unknown Artist ». Au rescan avec les vrais tags, le dossier
    /// retrouvait la même ligne et la retournait telle quelle : l'album restait
    /// « Unknown Artist » pour toujours, alors que toutes ses pistes portaient
    /// le bon artiste. L'album doit reprendre le vrai artiste.
    #[test]
    fn folder_album_reclaims_real_artist_over_unknown() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let unknown = artist_repo
            .create(&Artist::new(
                crate::db::artist_repo::UNKNOWN_ARTIST_NAME.into(),
            ))
            .unwrap();
        let real = artist_repo
            .create(&Artist::new("Shearwater".into()))
            .unwrap();
        let folder = "/music/Shearwater/The New World";

        // Premier passage : fichier en cours d'écriture, artiste inconnu.
        let created = repo
            .get_or_create_for_folder(folder, "The New World", unknown, None, None)
            .unwrap();
        assert_eq!(created.artist_id, Some(unknown));

        // Rescan avec les tags complets : même album, artiste réparé.
        let healed = repo
            .get_or_create_for_folder(folder, "The New World", real, None, None)
            .unwrap();
        assert_eq!(
            healed.id, created.id,
            "le dossier doit rester un seul album"
        );
        assert_eq!(healed.artist_id, Some(real));
        assert_eq!(healed.artist_name.as_deref(), Some("Shearwater"));

        // Et la réparation est persistée, pas seulement sur la valeur retournée.
        let reread = repo.get(created.id.unwrap()).unwrap().unwrap();
        assert_eq!(reread.artist_id, Some(real));
    }

    /// L'inverse ne doit jamais se produire : un fichier encore sans tags
    /// (résolu « Unknown Artist ») ne rétrograde pas un album déjà attribué.
    #[test]
    fn unknown_artist_never_downgrades_a_resolved_album() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let real = artist_repo
            .create(&Artist::new("Ben Harper".into()))
            .unwrap();
        let unknown = artist_repo
            .create(&Artist::new(
                crate::db::artist_repo::UNKNOWN_ARTIST_NAME.into(),
            ))
            .unwrap();
        let folder = "/music/Ben Harper/No Mercy In This Land";

        let created = repo
            .get_or_create_for_folder(folder, "No Mercy In This Land", real, None, None)
            .unwrap();
        let after = repo
            .get_or_create_for_folder(folder, "No Mercy In This Land", unknown, None, None)
            .unwrap();
        assert_eq!(after.id, created.id);
        assert_eq!(
            after.artist_id,
            Some(real),
            "l'album garde son vrai artiste"
        );
    }

    /// Deux vrais artistes en désaccord sur un même dossier : on ne tranche
    /// pas, l'album garde son attribution d'origine (comportement inchangé).
    #[test]
    fn a_real_artist_mismatch_is_left_alone() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let first = artist_repo.create(&Artist::new("Artist A".into())).unwrap();
        let second = artist_repo.create(&Artist::new("Artist B".into())).unwrap();
        let folder = "/music/A/Album";

        let created = repo
            .get_or_create_for_folder(folder, "Album", first, None, None)
            .unwrap();
        let after = repo
            .get_or_create_for_folder(folder, "Album", second, None, None)
            .unwrap();
        assert_eq!(after.id, created.id);
        assert_eq!(after.artist_id, Some(first));
    }

    /// Cas réduit de Philippe : l'album est figé sur l'artiste qui avait reçu
    /// le MBID vide, tandis que toutes ses pistes portent le bon artiste.
    #[test]
    fn a_healthy_rescan_repairs_an_empty_mbid_artist_collapse() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let wrong = artist_repo
            .create(&Artist::new("Classique - Saint-Saëns".into()))
            .unwrap();
        let right = artist_repo
            .create(&Artist::new("Anouar Brahem".into()))
            .unwrap();
        DbBackend::execute(
            &db,
            "UPDATE artists SET musicbrainz_id = '' WHERE id = ?",
            &[&wrong as &dyn ToSqlValue],
        )
        .unwrap();

        let album = album_repo
            .get_or_create_for_folder(
                "/music/Anouar Brahem/After the Last Sky",
                "After the Last Sky",
                wrong,
                Some(2025),
                None,
            )
            .unwrap();
        let album_id = album.id.unwrap();
        seed_track(&db, album_id, right, 1, "/music/Anouar/01.flac");
        seed_track(&db, album_id, right, 2, "/music/Anouar/02.flac");

        assert_eq!(album_repo.repair_empty_mbid_artist_collapses().unwrap(), 1);
        assert_eq!(
            album_repo.get(album_id).unwrap().unwrap().artist_id,
            Some(right)
        );
        assert_eq!(
            album_repo.repair_empty_mbid_artist_collapses().unwrap(),
            0,
            "la réparation doit être idempotente"
        );
    }

    /// Les quatre gardes qui empêchent une réattribution spéculative : absence
    /// de la signature MBID vide, compilation, artistes de pistes divergents et
    /// tag ALBUMARTIST explicite qui confirme l'attribution actuelle.
    #[test]
    fn ambiguous_album_artist_mismatches_are_never_repaired() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let current = artist_repo
            .create(&Artist::new("Album Artist".into()))
            .unwrap();
        let invalid_current = artist_repo
            .create(&Artist::new("Invalid Empty MBID Artist".into()))
            .unwrap();
        let track_a = artist_repo
            .create(&Artist::new("Track Artist A".into()))
            .unwrap();
        let track_b = artist_repo
            .create(&Artist::new("Track Artist B".into()))
            .unwrap();
        DbBackend::execute(
            &db,
            "UPDATE artists SET musicbrainz_id = '   ' WHERE id = ?",
            &[&invalid_current as &dyn ToSqlValue],
        )
        .unwrap();

        // MBID NULL : désaccord ordinaire, sans signature du collage #2458.
        let ordinary = album_repo
            .get_or_create_for_folder("/m/ordinary", "Ordinary", current, None, None)
            .unwrap();
        seed_track(&db, ordinary.id.unwrap(), track_a, 1, "/m/ordinary/01.flac");

        // Compilation : plusieurs artistes sont attendus par construction.
        let compilation = album_repo
            .get_or_create_for_folder("/m/compilation", "Compilation", invalid_current, None, None)
            .unwrap();
        album_repo
            .mark_compilation(compilation.id.unwrap())
            .unwrap();
        seed_track(
            &db,
            compilation.id.unwrap(),
            track_a,
            1,
            "/m/compilation/01.flac",
        );

        // Deux artistes de pistes : aucun consensus à utiliser comme preuve.
        let mixed = album_repo
            .get_or_create_for_folder("/m/mixed", "Mixed", invalid_current, None, None)
            .unwrap();
        seed_track(&db, mixed.id.unwrap(), track_a, 1, "/m/mixed/01.flac");
        seed_track(&db, mixed.id.unwrap(), track_b, 2, "/m/mixed/02.flac");

        // Le tag ALBUMARTIST confirme l'artiste d'album distinct de celui des
        // pistes : cas classique légitime, même si son vieux MBID est vide.
        let tagged = album_repo
            .get_or_create_for_folder("/m/tagged", "Tagged", invalid_current, None, None)
            .unwrap();
        seed_track_with_album_artist(
            &db,
            tagged.id.unwrap(),
            track_a,
            1,
            "/m/tagged/01.flac",
            Some("Invalid Empty MBID Artist"),
        );

        assert_eq!(album_repo.repair_empty_mbid_artist_collapses().unwrap(), 0);
        assert_eq!(
            album_repo
                .get(ordinary.id.unwrap())
                .unwrap()
                .unwrap()
                .artist_id,
            Some(current)
        );
        for album_id in [
            compilation.id.unwrap(),
            mixed.id.unwrap(),
            tagged.id.unwrap(),
        ] {
            assert_eq!(
                album_repo.get(album_id).unwrap().unwrap().artist_id,
                Some(invalid_current)
            );
        }
    }

    /// Même réparation sur le chemin MusicBrainz : un album retrouvé par son
    /// release id alors qu'il est resté « Unknown Artist » reprend le vrai
    /// artiste entrant.
    #[test]
    fn mbid_album_reclaims_real_artist_over_unknown() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let repo = AlbumRepo::new(db);
        let unknown = artist_repo
            .create(&Artist::new(
                crate::db::artist_repo::UNKNOWN_ARTIST_NAME.into(),
            ))
            .unwrap();
        let real = artist_repo
            .create(&Artist::new("Orquesta Akokán".into()))
            .unwrap();

        let created = repo
            .get_or_create_with_mbid("Orquesta Akokán", unknown, None, Some("mbid-akokan"))
            .unwrap();
        let healed = repo
            .get_or_create_with_mbid("Orquesta Akokán", real, None, Some("mbid-akokan"))
            .unwrap();
        assert_eq!(healed.id, created.id);
        assert_eq!(healed.artist_id, Some(real));
    }

    /// Semis en SQL brut par gros lots : les repos feraient des dizaines de
    /// milliers de requêtes (#1269).
    fn seed_grosse_bibliotheque(db: &SqliteDb, n_albums: usize, tracks_per_album: usize) {
        // Une bio enrichie de ~2 Ko par album : c'est ce que transporte le
        // tri quand la bibliothèque est passée par l'enrichissement.
        let bio = "b".repeat(2000);
        let mut sql = String::from("BEGIN;\n");
        for i in 1..=1000usize {
            sql.push_str(&format!(
                "INSERT INTO artists (id, name) VALUES ({i}, 'artiste {i}');\n"
            ));
        }
        db.execute_batch(&sql).unwrap();

        let mut sql = String::with_capacity(1 << 24);
        for a in 1..=n_albums {
            let artist = a % 1000 + 1;
            sql.push_str(&format!(
                "INSERT INTO albums (id, title, artist_id, year, bio) VALUES ({a}, 'album {a}', {artist}, {}, '{bio}');\n",
                1960 + (a % 60)
            ));
            if sql.len() > (1 << 22) {
                db.execute_batch(&sql).unwrap();
                sql.clear();
            }
        }
        db.execute_batch(&sql).unwrap();

        let mut sql = String::with_capacity(1 << 24);
        let mut tid = 0usize;
        for a in 1..=n_albums {
            let artist = a % 1000 + 1;
            for n in 1..=tracks_per_album {
                tid += 1;
                sql.push_str(&format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, file_mtime) \
                     VALUES ({tid}, 'piste {n}', {a}, {artist}, {n}, '/musique/{a}/{n}.flac', {});\n",
                    1_700_000_000.0 + (tid as f64)
                ));
                sql.push_str(&format!(
                    "INSERT INTO file_first_seen (file_path, first_seen_at) \
                     VALUES ('/musique/{a}/{n}.flac', {});\n",
                    1_700_000_000.0 + ((n_albums - a) as f64)
                ));
            }
            if sql.len() > (1 << 22) {
                db.execute_batch(&sql).unwrap();
                sql.clear();
            }
        }
        sql.push_str("COMMIT;\n");
        db.execute_batch(&sql).unwrap();
    }

    /// Pose `track_metadata` et tague `1/pas` des albums, en SQL brut (#2144).
    ///
    /// Une part MINORITAIRE, délibérément : c'est l'état réel des
    /// bibliothèques (« ça suppose que les tags soient présents sur une part
    /// suffisante », Bertrand, 15/08 — jamais mesuré), et c'est le cas le plus
    /// dur pour le tri, qui doit alors départager une majorité de NULL.
    fn seed_tags_dr(db: &SqliteDb, n_albums: usize, tracks_per_album: usize, pas: usize) {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS track_metadata (
                 track_id INTEGER NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 PRIMARY KEY (track_id, key)
             );",
        )
        .unwrap();
        // Même index que la migration 34 : sans lui la mesure serait plus
        // favorable que la production ne le sera jamais.
        db.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_track_metadata_key ON track_metadata(key);",
        )
        .unwrap();
        let mut sql = String::from("BEGIN;\n");
        for a in (1..=n_albums).step_by(pas) {
            // La 1re piste de l'album `a` porte l'identifiant (a-1)*tpa + 1.
            let tid = (a - 1) * tracks_per_album + 1;
            sql.push_str(&format!(
                "INSERT INTO track_metadata (track_id, key, value) VALUES ({tid}, 'dr_album', '{}');\n",
                (a / pas) % 20
            ));
        }
        sql.push_str("COMMIT;\n");
        db.execute_batch(&sql).unwrap();
    }

    /// #2144 — contre-épreuve de coût : trier ou filtrer par DR doit coûter du
    /// même ordre qu'une page du tri trivial, PAS un multiple.
    ///
    /// C'est la garde qui protège l'acquis de #1269 : le DR est lu dans
    /// `track_metadata`, et la forme naïve — `(SELECT … WHERE t.album_id =
    /// a.id)` dans l'ORDER BY — serait ré-évaluée pour CHACUNE des 45 000
    /// lignes triées, exactement la sous-requête corrélée que #1269 vient de
    /// retirer du tri par défaut. La jointure groupée ne la paie qu'une fois.
    #[test]
    fn trier_et_filtrer_par_dr_coute_comme_le_tri_trivial_2144() {
        let db = test_db();
        let repo = AlbumRepo::new(db.clone());
        seed_grosse_bibliotheque(&db, 10_000, 2);
        seed_tags_dr(&db, 10_000, 2, 10);

        let chrono = |sort: &str, dr: Option<DrRange>, attendu: usize| -> std::time::Duration {
            (0..3)
                .map(|_| {
                    let t0 = std::time::Instant::now();
                    let page = repo
                        .list_filtered(2000, 0, sort, "asc", None, None, None, true, dr)
                        .unwrap();
                    assert_eq!(page.len(), attendu);
                    t0.elapsed()
                })
                .min()
                .unwrap()
        };

        let t_id = chrono("id", None, 2000);
        let t_dr = chrono("dynamic_range", None, 2000);
        // 1000 albums tagués, DR = a % 20 : la moitié tombe au-dessus de 10.
        let tranche = DrRange::new(Some(10), None);
        let t_tranche = chrono("dynamic_range", tranche, 500);
        eprintln!("contre-épreuve #2144 : id={t_id:?} tri_dr={t_dr:?} tranche_dr={t_tranche:?}");

        let plafond = t_id.max(std::time::Duration::from_millis(5)) * 8;
        assert!(
            t_dr < plafond,
            "le tri par DR coûte {t_dr:?} pour une page contre {t_id:?} en tri \
             id : la page re-scanne `track_metadata` par ligne (#2144/#1269)"
        );
        assert!(
            t_tranche < plafond,
            "la tranche de DR coûte {t_tranche:?} contre {t_id:?} en tri id"
        );
    }

    /// #2144 — mesure : 45 000 albums, tri et tranche de DR à trois offsets.
    /// Lancée à la main (`--ignored --nocapture`), comme son aînée #1269.
    #[test]
    #[ignore = "mesure #2144, lancement manuel"]
    fn bench_dynamic_range_45000_albums() {
        let db = test_db();
        let repo = AlbumRepo::new(db.clone());
        seed_grosse_bibliotheque(&db, 45_000, 4);
        seed_tags_dr(&db, 45_000, 4, 10);

        for offset in [0i64, 22_000, 44_000] {
            let t0 = std::time::Instant::now();
            let page = repo
                .list_filtered(
                    2000,
                    offset,
                    "dynamic_range",
                    "asc",
                    None,
                    None,
                    None,
                    true,
                    None,
                )
                .unwrap();
            eprintln!(
                "tri dynamic_range offset={offset}: {} albums en {:?}",
                page.len(),
                t0.elapsed()
            );
            assert!(!page.is_empty());
        }
        let r = DrRange::new(Some(10), Some(19)).unwrap();
        for offset in [0i64, 1000] {
            let t0 = std::time::Instant::now();
            let page = repo
                .list_filtered(
                    2000,
                    offset,
                    "dynamic_range",
                    "asc",
                    None,
                    None,
                    None,
                    true,
                    Some(r),
                )
                .unwrap();
            eprintln!(
                "tranche DR10-19 offset={offset}: {} albums en {:?}",
                page.len(),
                t0.elapsed()
            );
        }
        let t0 = std::time::Instant::now();
        eprintln!(
            "count_in_dr_range: {} en {:?}",
            repo.count_in_dr_range(r, true).unwrap(),
            t0.elapsed()
        );
        // Témoin : tri bon marché (id), même volume.
        let t0 = std::time::Instant::now();
        let page = repo
            .list_filtered(2000, 0, "id", "asc", None, None, None, true, None)
            .unwrap();
        eprintln!("id offset=0: {} albums en {:?}", page.len(), t0.elapsed());
    }

    /// #1269 — contre-épreuve : une page du tri par défaut (`added_at`) doit
    /// coûter du même ordre qu'une page du tri trivial (`id`), pas 8 fois
    /// plus. Mesuré sur cette base (10 000 albums, bios de 2 Ko) : l'ancienne
    /// forme — sous-requête corrélée ré-évaluée PAR LIGNE et lignes complètes
    /// traînées dans le trieur à chaque page — coûtait 17,6× le tri trivial ;
    /// la forme en deux temps (tri de lignes étroites, matérialisation de la
    /// seule page) coûte 3,1×. Meilleure de 3 passes de chaque côté, pour
    /// amortir le bruit d'une machine de CI chargée.
    #[test]
    fn le_tri_par_defaut_coute_comme_le_tri_trivial_1269() {
        let db = test_db();
        let repo = AlbumRepo::new(db.clone());
        seed_grosse_bibliotheque(&db, 10_000, 2);

        let chrono = |sort: &str| -> std::time::Duration {
            (0..3)
                .map(|_| {
                    let t0 = std::time::Instant::now();
                    let page = repo
                        .list_filtered(2000, 6000, sort, "asc", None, None, None, true, None)
                        .unwrap();
                    assert_eq!(page.len(), 2000);
                    t0.elapsed()
                })
                .min()
                .unwrap()
        };

        let t_id = chrono("id");
        let t_added = chrono("added_at");
        eprintln!("contre-épreuve #1269 : id={t_id:?} added_at={t_added:?}");
        assert!(
            t_added < t_id.max(std::time::Duration::from_millis(5)) * 8,
            "le tri added_at coûte {t_added:?} pour une page, contre {t_id:?} \
             en tri id : la page re-trie ou re-scanne toute la bibliothèque (#1269)"
        );
    }

    /// #1269 — mesure : bibliothèque de 45 000 albums, tri par défaut
    /// (`added_at`). Lancé à la main (`--ignored --nocapture`), jamais en CI.
    #[test]
    #[ignore = "mesure #1269, lancement manuel"]
    fn bench_added_at_45000_albums() {
        let db = test_db();
        let repo = AlbumRepo::new(db.clone());

        seed_grosse_bibliotheque(&db, 45_000, 4);

        // Le client iOS charge TOUT par pages de 2000, tri par défaut.
        for offset in [0i64, 22_000, 44_000] {
            let t0 = std::time::Instant::now();
            let page = repo
                .list_filtered(
                    2000, offset, "added_at", "asc", None, None, None, true, None,
                )
                .unwrap();
            eprintln!(
                "added_at offset={offset}: {} albums en {:?}",
                page.len(),
                t0.elapsed()
            );
            assert!(!page.is_empty());
        }
        // Témoin : tri bon marché (id), même volume.
        let t0 = std::time::Instant::now();
        let page = repo
            .list_filtered(2000, 0, "id", "asc", None, None, None, true, None)
            .unwrap();
        eprintln!("id offset=0: {} albums en {:?}", page.len(), t0.elapsed());
    }
}
