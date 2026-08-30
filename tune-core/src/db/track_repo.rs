use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
pub use super::facet_filter::TrackFilter;
use super::facet_filter::{
    Placeholders, any_of, favorite_condition, hidden_tracks_excluded, untagged_condition,
};
use super::models::Track;
use super::sqlite::SqliteDb;
use crate::TuneError;

/// Build the `LIKE` pattern that matches every track whose file lives under
/// `prefix` (recursively). Trailing separators are trimmed so a library pointed
/// at a share/drive root doesn't produce a doubled separator that matches
/// nothing (same trap handled in `browse.rs`). The server's `MAIN_SEPARATOR` is
/// the separator stored in `tracks.file_path` (paths are absolute local paths on
/// the scanning host), so both the flat filter and the folder facet agree.
///
/// Le préfixe est replié en **NFC**, parce que c'est la forme sous laquelle le
/// scanner écrit `tracks.file_path` (`scanner::walker` et `routes/system/scan`
/// appellent tous deux `.nfc()` avant d'insérer). Un préfixe qui vient du
/// *disque* — et non de la base — peut arriver en **NFD** : un dossier accentué
/// créé côté NAS, ou copié depuis macOS, n'existe qu'en forme décomposée, et
/// c'est cette forme-là que `resolve_browse_path` rend puisque c'est la seule
/// que le système de fichiers accepte d'ouvrir. Les deux chaînes s'affichent à
/// l'identique et ne partagent pas un octet : sans ce repli, le `LIKE` ne
/// ramène **aucune** ligne pour un dossier pourtant scanné, et l'écran annonce
/// un répertoire vide.
///
/// Sur un préfixe déjà NFC — tout ce qui sort de la base — le repli est un
/// no-op par construction.
pub fn folder_like_pattern(prefix: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let sep = std::path::MAIN_SEPARATOR;
    let base: String = prefix.trim_end_matches(['/', '\\']).nfc().collect();
    format!("{base}{sep}%")
}

/// SQL suffix that must follow every `LIKE` whose pattern is a **file path**.
///
/// Postgres treats a backslash inside a `LIKE` pattern as the default escape
/// character; SQLite has no default escape character at all. Folder patterns
/// are built from absolute paths, so on a Windows host the pattern reads
/// `G:\Blues 2\%` — Postgres consumes the trailing `\%` as a *literal* percent
/// sign, the pattern degrades to the literal string `G:Blues 2%`, and it
/// matches no row. Every folder then reports "0 pistes" while the library is
/// perfectly scanned (JF, Windows + Postgres: all four roots empty in
/// Répertoires, and the data-derived fallback root fell to zero too because it
/// reuses the same pattern).
///
/// `ESCAPE ''` selects *no* escape character, which is exactly SQLite's
/// behaviour — so both engines now read the backslash literally. SQLite rejects
/// an empty ESCAPE string, hence the bare `LIKE` there: its semantics are
/// already the target ones, so this is a no-op on SQLite by construction.
///
/// Known and unchanged on both engines: `%` and `_` inside a real folder name
/// still act as wildcards (over-match, never under-match). Escaping them would
/// require an escape character, which is what we are deliberately giving up.
pub fn like_escape_clause(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => " ESCAPE ''",
        Engine::Sqlite => "",
    }
}

/// The longest common directory prefix of all `tracks.file_path` — the real
/// library root inferred from the data. Fallback for the folder views (Oxygen
/// facet, browse "Répertoires") when `music_dirs` is stale and its configured
/// roots match no stored path (the browse_root_zero_tracks drift: e.g. .18 set
/// to /mnt/music while files live under /data/music). For sorted strings
/// LCP(all) == LCP(min, max), so two aggregates suffice — no full scan. Returns
/// the directory (trailing separator dropped), or None if the library is empty
/// or the common prefix has no separator.
pub fn derive_common_root(backend: &dyn DbBackend) -> Option<String> {
    let agg = |f: &str| -> Option<String> {
        backend
            .query_one(
                &format!(
                    "SELECT {f}(file_path) FROM tracks \
                     WHERE file_path IS NOT NULL AND file_path <> ''"
                ),
                &[],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_string()))
    };
    let (min, max) = (agg("MIN")?, agg("MAX")?);
    let lcp = common_prefix(&min, &max);
    // Trim back to the last separator → a directory (handles both / and \ so it
    // works whichever separator the scanning host stored).
    let idx = lcp.rfind(['/', '\\'])?;
    let root = &lcp[..idx];
    (!root.is_empty()).then(|| root.to_string())
}

/// Longest common (char-boundary-safe) prefix of two strings.
pub(crate) fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let mut end = 0;
    for ((i, ca), cb) in a.char_indices().zip(b.chars()) {
        if ca != cb {
            break;
        }
        end = i + ca.len_utf8();
    }
    &a[..end]
}

#[cfg(test)]
mod common_root_tests {
    use super::common_prefix;

    #[test]
    fn common_prefix_is_the_shared_directory() {
        // Real .18 case: min/max of the library share "/data/music/".
        assert_eq!(
            common_prefix("/data/music/10. x.flac", "/data/music/V_DSF/y.dsf"),
            "/data/music/"
        );
        assert_eq!(common_prefix("/a/b", "/a/c"), "/a/");
        assert_eq!(common_prefix("/x", "/y"), "/");
        assert_eq!(common_prefix("same", "same"), "same");
        // Multibyte: must cut on a char boundary, never mid-codepoint.
        assert_eq!(common_prefix("/muské/a", "/muskà/b"), "/musk");
    }
}

#[cfg(test)]
mod like_escape_tests {
    use super::{Engine, like_escape_clause};

    /// Regression guard for the Windows + Postgres "Dossier vide" bug: a folder
    /// pattern built from `G:\Blues 2` ends in `\%`, which Postgres reads as an
    /// escaped — hence literal — percent sign unless the escape mechanism is
    /// switched off. Verified against PostgreSQL 15: without the clause the
    /// count is 0, with it the count is right. Deleting this suffix silently
    /// empties the Répertoires view and the Oxygen folder drill-down for every
    /// Windows user on Postgres — and only for them, which is why it went
    /// unnoticed.
    #[test]
    fn postgres_disables_the_backslash_escape() {
        assert_eq!(like_escape_clause(Engine::Postgres), " ESCAPE ''");
    }

    /// SQLite has no default escape character, so a bare LIKE already reads the
    /// backslash literally — and it *rejects* an empty ESCAPE string
    /// ("ESCAPE expression must be a single character"). The clause must stay
    /// empty here: this is a correctness constraint, not a style choice.
    #[test]
    fn sqlite_keeps_the_bare_like() {
        assert_eq!(like_escape_clause(Engine::Sqlite), "");
    }
}

/// Engine-agnostic SQL builders for track_repo.
///
/// Complex dynamic queries (search() FTS5, list_doubtful() aggregate,
/// deduplicate(), random_ids() with RANDOM()) retain SQLite-specific
/// fragments behind TODO comments; phase 4 swaps them for PG
/// equivalents via dialect helpers.
pub mod sql {
    use super::SqlDialect;

    pub fn select_track() -> &'static str {
        // `album_artist` falls back to the album's canonical artist (`albums.
        // artist_id`, e.g. "Various Artists" for a compilation) when the per-file
        // ALBUMARTIST tag is missing. Without this, the Oxygen "by genre" view —
        // which groups a *filtered subset* of an album's tracks client-side — has
        // no album_artist to key on and shows track 1's artist for compilations
        // whose files carry no ALBUMARTIST tag (Bilou). The column keeps its
        // position, so row parsing is unchanged.
        "SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, COALESCE(NULLIF(t.album_artist, ''), aal.name), t.disc_number, t.disc_subtitle, t.track_number, t.duration_ms, t.file_path, t.format, t.sample_rate, t.bit_depth, t.channels, t.file_mtime, t.file_size, t.audio_hash, t.source, t.source_id, t.isrc, t.genre, t.composer, t.year, t.bpm, t.label, t.musicbrainz_recording_id, COALESCE(t.cover_path, al.cover_path), t.genres, t.comments FROM tracks t LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id LEFT JOIN artists aal ON al.artist_id = aal.id"
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("{} WHERE t.id = {}", select_track(), d.placeholder(1))
    }

    pub fn get_by_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.file_path = {}",
            select_track(),
            d.placeholder(1)
        )
    }

    const INSERT_COLS: &str = "title, album_id, artist_id, album_artist, disc_number, disc_subtitle, track_number, duration_ms, file_path, format, sample_rate, bit_depth, channels, file_mtime, file_size, audio_hash, source, source_id, isrc, genre, genres, composer, year, bpm, label, musicbrainz_recording_id, comments, cover_path";

    pub fn insert<D: SqlDialect>(d: &D) -> String {
        let placeholders: Vec<String> = (1..=28).map(|i| d.placeholder(i)).collect();
        format!(
            "INSERT INTO tracks ({INSERT_COLS}) VALUES ({})",
            placeholders.join(", ")
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET title = {}, album_id = {}, artist_id = {}, album_artist = {}, disc_number = {}, disc_subtitle = {}, track_number = {}, duration_ms = {}, file_path = {}, format = {}, sample_rate = {}, bit_depth = {}, channels = {}, file_mtime = {}, file_size = {}, audio_hash = {}, genre = {}, genres = {}, composer = {}, year = {}, bpm = {}, label = {}, musicbrainz_recording_id = {}, comments = {} WHERE id = {}",
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
            d.placeholder(24),
            d.placeholder(25),
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM tracks WHERE id = {}", d.placeholder(1))
    }

    pub fn delete_all() -> &'static str {
        "DELETE FROM tracks"
    }

    pub fn delete_by_path<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM tracks WHERE file_path = {}", d.placeholder(1))
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM tracks"
    }

    /// Compteur de la VUE pistes : exclut les pistes d'albums masqués, comme
    /// la liste qu'il pagine (#1391). `count()` reste le compte COMPLET.
    pub fn count_visible() -> String {
        format!(
            "SELECT COUNT(*) FROM tracks t WHERE {}",
            crate::db::facet_filter::hidden_tracks_excluded()
        )
    }

    pub fn list_paginated<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} ORDER BY t.id LIMIT {} OFFSET {}",
            select_track(),
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn list_by_album<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.album_id = {} ORDER BY CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER), t.title",
            select_track(),
            d.placeholder(1)
        )
    }

    /// Vue « pistes de l'artiste » : les pistes d'un album masqué en sortent
    /// aussi (#1391) — contrairement à `list_by_album`, qui reste ENTIER pour
    /// que l'album masqué demeure jouable depuis une file ou une playlist.
    pub fn list_by_artist<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.artist_id = {} AND {} ORDER BY al.year, al.title, CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER)",
            select_track(),
            d.placeholder(1),
            crate::db::facet_filter::hidden_tracks_excluded()
        )
    }

    pub fn list_by_path<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE t.file_path = {}",
            select_track(),
            d.placeholder(1)
        )
    }

    pub fn update_mtime_and_size<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET file_mtime = {}, file_size = {} WHERE file_path = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn update_audio_hash<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET audio_hash = {} WHERE file_path = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_duration<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET duration_ms = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    // ─── Track metadata column helpers (see migration 003) ─────────

    pub fn get_synced_lyrics<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT synced_lyrics FROM tracks WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_synced_lyrics<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET synced_lyrics = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_trailing_silence<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT trailing_silence_ms FROM tracks WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_trailing_silence<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET trailing_silence_ms = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_waveform<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT waveform_json FROM tracks WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_waveform<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET waveform_json = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn set_acoustid<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tracks SET acoustid_fingerprint = {}, acoustid_confidence = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn list_unidentified<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE (t.title LIKE 'Track %' OR t.title LIKE 'Unknown%' \
             OR ar.name = 'Unknown Artist' OR ar.name IS NULL) \
             AND t.acoustid_fingerprint IS NULL \
             AND t.file_path IS NOT NULL \
             ORDER BY t.id LIMIT {}",
            select_track(),
            d.placeholder(1)
        )
    }

    pub fn get_credits<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, track_id, artist_id, artist_name, role, instrument, position \
             FROM track_credits WHERE track_id = {} ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn get_all_paths() -> &'static str {
        "SELECT file_path FROM tracks WHERE source = 'local' AND file_path IS NOT NULL"
    }

    pub fn get_all_local_file_info() -> &'static str {
        "SELECT id, file_path, file_mtime, file_size FROM tracks WHERE source = 'local' AND file_path IS NOT NULL"
    }

    pub fn get_existing_audio_hash_album_pairs() -> &'static str {
        "SELECT audio_hash, album_id FROM tracks \
         WHERE source = 'local' AND audio_hash IS NOT NULL AND album_id IS NOT NULL"
    }

    pub fn get_existing_audio_hash_album_paths() -> &'static str {
        "SELECT audio_hash, album_id, file_path FROM tracks \
         WHERE source = 'local' AND audio_hash IS NOT NULL \
           AND album_id IS NOT NULL AND file_path IS NOT NULL"
    }

    /// Engine-agnostic search.
    /// Le OU des critères est PARENTHÉSÉ pour recevoir le filtre « pas dans
    /// un album masqué » en ET — appliqué APRÈS la passe FTS, les index
    /// `tracks_fts` contiennent tout (#1391).
    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE ({} OR LOWER(unaccent(ar.name)) LIKE LOWER(unaccent({})) OR LOWER(unaccent(t.genre)) LIKE LOWER(unaccent({})) OR LOWER(unaccent(t.composer)) LIKE LOWER(unaccent({})) OR CAST(al.year AS TEXT) = {}) AND {} LIMIT {}",
            select_track(),
            d.fts_where("tracks", "t", &d.placeholder(1)),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            crate::db::facet_filter::hidden_tracks_excluded(),
            d.placeholder(6),
        )
    }
}

/// Collapse content-duplicate tracks for DISPLAY, preserving order.
///
/// When a user keeps real duplicate files on disk (e.g. a track copied from a
/// NAS to a local folder, or a folder duplicated), the same recording gets one
/// row per `file_path` — so an album lists a track two or three times. This
/// hides those extra copies in list views WITHOUT touching the files or the
/// shared query methods (which internal callers — playlist matching, tag
/// writing, conversion — still need to see every row).
///
/// Display collapsing deliberately ignores `audio_hash`: a sampled candidate
/// must not make a real track disappear from an album or artist view. The
/// legacy presentation key remains album + disc + track + case-insensitive
/// title. The first occurrence in input order is kept.
pub fn dedup_display_tracks(tracks: Vec<Track>) -> Vec<Track> {
    let mut seen: HashSet<(Option<i64>, String)> = HashSet::new();
    let mut out = Vec::with_capacity(tracks.len());
    for t in tracks {
        let key = (
            t.album_id,
            format!(
                "m:{}/{}/{}",
                t.disc_number,
                t.track_number,
                t.title.trim().to_lowercase()
            ),
        );
        if seen.insert(key) {
            out.push(t);
        }
    }
    out
}

/// Nombre d'ids inlinés par requête `WHERE t.id IN (…)`.
///
/// Les ids sont des `i64` issus de nos propres requêtes : les inliner ne
/// consomme **aucun** paramètre lié, donc aucune requête ne peut atteindre la
/// limite de paramètres d'un moteur — SQLite `SQLITE_MAX_VARIABLE_NUMBER`
/// (999 avant 3.32, 32766 depuis) ni PostgreSQL (65535 paramètres par message
/// Bind, le champ de comptage étant un entier 16 bits non signé).
///
/// Reste la limite de *longueur* d'instruction de SQLite (`SQLITE_MAX_SQL_LENGTH`,
/// 1 Mo par défaut) : d'où le découpage. 5000 ids × 20 caractères ≈ 100 Ko au
/// pire, soit un ordre de grandeur de marge. Même valeur que la matérialisation
/// de page d'`AlbumRepo::list_filtered` (#1269), pour ne pas multiplier les
/// constantes de découpage dans le dépôt.
pub(crate) const ID_INLINE_BATCH: usize = 5_000;

pub struct TrackRepo {
    db: Arc<dyn DbBackend>,
}

impl TrackRepo {
    pub fn backend(&self) -> &dyn DbBackend {
        &*self.db
    }
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

    // ─── Group A: simple ports via DbBackend ──────────────────────
    //
    // Internal methods use TuneError for typed errors (Db, NotFound, etc.).
    // Public API returns Result<T, String> for backward-compat with callers.
    // The bridge: `.map_err(TuneError::from)` converts TuneError → String at the boundary.

    fn get_inner(&self, id: i64) -> Result<Option<Track>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self
            .db
            .query_one(&sql, &params)
            .map_err(TuneError::Db)?
            .as_ref()
            .map(row_to_track))
    }

    pub fn get(&self, id: i64) -> Result<Option<Track>, TuneError> {
        self.get_inner(id).map_err(TuneError::from)
    }

    pub fn get_by_path(&self, file_path: &str) -> Result<Option<Track>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_path, sql::get_by_path);
        let params: [&dyn ToSqlValue; 1] = [&file_path];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_track))
    }

    fn create_inner(&self, track: &Track) -> Result<i64, TuneError> {
        let sql = self.dialect_sql(sql::insert, sql::insert);
        let params: [&dyn ToSqlValue; 28] = [
            &track.title,
            &track.album_id,
            &track.artist_id,
            &track.album_artist,
            &track.disc_number,
            &track.disc_subtitle,
            &track.track_number,
            &track.duration_ms,
            &track.file_path,
            &track.format,
            &track.sample_rate,
            &track.bit_depth,
            &track.channels,
            &track.file_mtime,
            &track.file_size,
            &track.audio_hash,
            &track.source,
            &track.source_id,
            &track.isrc,
            &track.genre,
            &track.genres,
            &track.composer,
            &track.year,
            &track.bpm,
            &track.label,
            &track.musicbrainz_recording_id,
            &track.comments,
            &track.cover_path,
        ];
        // Capture the new track id atomically, BEFORE the file_first_seen
        // insert below — otherwise `last_insert_rowid()` at the end returns that
        // side-table's row id for any newly-seen file (intervening-insert race,
        // audit item 5).
        let id = self
            .db
            .execute_returning_id(&sql, &params)
            .map_err(TuneError::Db)?;

        // Record the library "first seen" timestamp for local files, keyed by
        // path in a side table that survives a full rescan (delete_all wipes
        // tracks/albums but not file_first_seen). Best-effort: never fail track
        // creation over this. Streaming tracks (http URLs / no path) are skipped.
        if let Some(path) = track.file_path.as_deref() {
            if !path.is_empty() && !path.starts_with("http") {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let fs_sql = match self.db.engine() {
                    Engine::Postgres => {
                        "INSERT INTO file_first_seen (file_path, first_seen_at) VALUES ($1, $2) ON CONFLICT (file_path) DO NOTHING"
                    }
                    Engine::Sqlite => {
                        "INSERT OR IGNORE INTO file_first_seen (file_path, first_seen_at) VALUES (?, ?)"
                    }
                };
                let fs_params: [&dyn ToSqlValue; 2] = [&path, &now];
                let _ = self.db.execute(fs_sql, &fs_params);
            }
        }

        Ok(id)
    }

    pub fn create(&self, track: &Track) -> Result<i64, TuneError> {
        self.create_inner(track).map_err(TuneError::from)
    }

    fn update_inner(&self, track: &Track) -> Result<(), TuneError> {
        let id = track
            .id
            .ok_or_else(|| TuneError::NotFound("track has no id".into()))?;
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 25] = [
            &track.title,
            &track.album_id,
            &track.artist_id,
            &track.album_artist,
            &track.disc_number,
            &track.disc_subtitle,
            &track.track_number,
            &track.duration_ms,
            &track.file_path,
            &track.format,
            &track.sample_rate,
            &track.bit_depth,
            &track.channels,
            &track.file_mtime,
            &track.file_size,
            &track.audio_hash,
            &track.genre,
            &track.genres,
            &track.composer,
            &track.year,
            &track.bpm,
            &track.label,
            &track.musicbrainz_recording_id,
            &track.comments,
            &id,
        ];
        self.db.execute(&sql, &params).map_err(TuneError::Db)?;
        Ok(())
    }

    pub fn update(&self, track: &Track) -> Result<(), TuneError> {
        self.update_inner(track).map_err(TuneError::from)
    }

    pub fn delete(&self, id: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        // Drop any queue entry referencing this track. The FK ON DELETE CASCADE
        // is present on a fresh schema but absent on DBs created by the
        // unified-queue migration, so a stale queue_items row would otherwise
        // linger and later break set_queue with a FK error (JP Borderies).
        let ph = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let _ = self.db.execute(
            &format!("DELETE FROM queue_items WHERE track_id = {ph}"),
            &params,
        );
        Ok(())
    }

    pub fn delete_all(&self) -> Result<u64, TuneError> {
        // 4 sequential DELETEs — wrap in write_tx for atomicity.
        let mut count: u64 = 0;
        let count_ref = &mut count;
        self.db.write_tx(&mut |tx| {
            *count_ref = tx.execute(sql::delete_all(), &[])? as u64;
            let _ = tx.execute("DELETE FROM albums", &[]);
            let _ = tx.execute("DELETE FROM artists", &[]);
            let _ = tx.execute("DELETE FROM track_credits", &[]);
            // Clear local (track-backed) queue entries too — CASCADE is missing
            // on migrated DBs, so wiping the library must not leave a queue
            // pointing at deleted tracks (JP Borderies).
            let _ = tx.execute("DELETE FROM queue_items WHERE track_id IS NOT NULL", &[]);
            Ok(())
        })?;
        Ok(count)
    }

    pub fn delete_by_path(&self, file_path: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::delete_by_path, sql::delete_by_path);
        let params: [&dyn ToSqlValue; 1] = [&file_path];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, TuneError> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<Track>, TuneError> {
        let sql = format!(
            "{} ORDER BY LOWER(ar.name), LOWER(al.title), CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER) LIMIT {} OFFSET {}",
            sql::select_track(),
            match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(1),
                Engine::Postgres => PostgresDialect.placeholder(1),
            },
            match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(2),
                Engine::Postgres => PostgresDialect.placeholder(2),
            }
        );
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    /// Chemin NON facetté de `GET /library/tracks` : mêmes lignes et même
    /// ordre que [`Self::list`], moins les pistes d'albums masqués — le
    /// miroir du prédicat que `list_filtered` pose toujours, sans quoi la vue
    /// par défaut fuirait ce que la vue facettée cache (#1391). `list` reste
    /// ENTIER pour la maintenance (export, résolutions internes).
    pub fn list_visible(&self, limit: i64, offset: i64) -> Result<Vec<Track>, TuneError> {
        let sql = format!(
            "{} WHERE {} ORDER BY LOWER(ar.name), LOWER(al.title), CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER) LIMIT {} OFFSET {}",
            sql::select_track(),
            hidden_tracks_excluded(),
            match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(1),
                Engine::Postgres => PostgresDialect.placeholder(1),
            },
            match self.db.engine() {
                Engine::Sqlite => SqliteDialect.placeholder(2),
                Engine::Postgres => PostgresDialect.placeholder(2),
            }
        );
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    /// Compteur de la vue pistes : exclut comme [`Self::list_visible`].
    pub fn count_visible(&self) -> Result<i64, TuneError> {
        match self.db.query_one(&sql::count_visible(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    /// Filtered track listing with optional WHERE clauses.
    ///
    /// **Sémantique des facettes (#2168)** : plusieurs valeurs DANS une facette
    /// se combinent en **OU** (`format = aiff OU flac`) ; deux facettes
    /// différentes se combinent en **ET** (`format = flac ET genre = jazz`).
    /// Une facette dont la liste est vide ne produit AUCUN prédicat — ni
    /// `IN ()`, ni un `1 = 1` qui rendrait la bibliothèque entière.
    ///
    /// Returns (items, total_matching_count).
    pub fn list_filtered(
        &self,
        f: &TrackFilter,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Track>, i64), TuneError> {
        let engine = self.db.engine();
        // Un SEUL compteur de marqueurs pour tout le WHERE : en SQLite ils
        // s'écrivent tous `?` et seul l'ORDRE de liaison compte, donc chaque
        // valeur doit être empilée exactement quand son marqueur est demandé.
        let mut ph = Placeholders::new(engine);

        let mut conditions: Vec<String> = Vec::new();
        let mut owned_params: Vec<SqlValue> = Vec::new();

        // Genre : la colonne `t.genre` OU le tableau JSON `t.genres`. Avec
        // plusieurs genres sélectionnés, les deux tests s'étendent ensemble —
        // et les valeurs sont empilées dans l'ordre où les marqueurs sortent :
        // d'abord les N du `IN`, puis les N motifs `LIKE`.
        if !f.genres.is_empty() {
            let n = f.genres.len();
            let in_part = ph
                .in_list_ci("t.genre", n)
                .expect("liste non vide déjà vérifiée");
            let like_part = (0..n)
                .map(|_| format!("t.genres LIKE {}", ph.take()))
                .collect::<Vec<_>>()
                .join(" OR ");
            conditions.push(format!("({in_part} OR {like_part})"));
            for g in &f.genres {
                owned_params.push(SqlValue::Text(g.clone()));
            }
            for g in &f.genres {
                owned_params.push(SqlValue::Text(format!("%\"{g}\"%")));
            }
        }

        if let Some(c) = ph.in_list("t.year", f.years.len()) {
            conditions.push(c);
            for y in &f.years {
                owned_params.push(SqlValue::Int(*y));
            }
        }

        if let Some(c) = ph.in_list_ci("t.format", f.formats.len()) {
            conditions.push(c);
            for v in &f.formats {
                owned_params.push(SqlValue::Text(v.clone()));
            }
        }

        if let Some(c) = ph.in_list("t.sample_rate", f.sample_rates.len()) {
            conditions.push(c);
            for v in &f.sample_rates {
                owned_params.push(SqlValue::Int(*v));
            }
        }

        if let Some(c) = ph.in_list("t.bit_depth", f.bit_depths.len()) {
            conditions.push(c);
            for v in &f.bit_depths {
                owned_params.push(SqlValue::Int(*v));
            }
        }

        if let Some(c) = ph.in_list("t.source", f.sources.len()) {
            conditions.push(c);
            for v in &f.sources {
                owned_params.push(SqlValue::Text(v.clone()));
            }
        }

        if let Some(c) = ph.or_like_ci("t.label", f.labels.len()) {
            conditions.push(c);
            for v in &f.labels {
                owned_params.push(SqlValue::Text(format!("%{v}%")));
            }
        }

        if let Some(c) = ph.or_like_ci("t.composer", f.composers.len()) {
            conditions.push(c);
            for v in &f.composers {
                owned_params.push(SqlValue::Text(format!("%{v}%")));
            }
        }

        // The artist name lives on the joined `artists` table (tracks stores
        // only artist_id) — `tracks` has no artist_name column, so the old
        // `t.artist_name` predicate raised a SQL error that list_tracks
        // swallowed into an empty result: clicking an Oxygen "Artistes" facet
        // returned zero tracks (forum #1189). The base query + count both
        // LEFT JOIN artists ar, so filter on ar.name.
        if let Some(c) = ph.in_list("ar.name", f.artists.len()) {
            conditions.push(c);
            for v in &f.artists {
                owned_params.push(SqlValue::Text(v.clone()));
            }
        }

        // Extended-tag filters via the open `track_metadata` k/v store. The key
        // is a fixed literal; only the values are bound parameters.
        for (values, key) in [
            (&f.countries, "release_country"),
            (&f.moods, "mood"),
            (&f.source_medias, "source_media"),
        ] {
            if let Some(c) = ph.in_list("tm.value", values.len()) {
                conditions.push(format!(
                    "EXISTS (SELECT 1 FROM track_metadata tm \
                     WHERE tm.track_id = t.id AND tm.key = '{key}' AND {c})"
                ));
                for v in values {
                    owned_params.push(SqlValue::Text(v.clone()));
                }
            }
        }

        // Folder facet (Oxygen drill-down): restrict to tracks whose file lives
        // under the selected directory subtree. The current breadcrumb path IS
        // the filter — recursive so a parent folder includes its sub-folders.
        // Reste MONOVALUÉ : un chemin est une position dans un arbre, pas une
        // valeur parmi d'autres (l'interface n'offre qu'un fil d'Ariane).
        if let Some(fld) = f.folder.as_deref().filter(|s| !s.is_empty()) {
            conditions.push(format!(
                "t.file_path LIKE {}{}",
                ph.take(),
                like_escape_clause(engine)
            ));
            owned_params.push(SqlValue::Text(folder_like_pattern(fld)));
        }

        // Album rating (profile 1): tracks inherit their album's rating.
        if let Some(c) = ph.in_list("arr.rating", f.ratings.len()) {
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM album_ratings arr \
                 WHERE arr.album_id = t.album_id AND arr.profile_id = 1 AND {c})"
            ));
            for v in &f.ratings {
                owned_params.push(SqlValue::Int(*v));
            }
        }

        // Manual collection: album ids are our own i64s (parsed from the
        // collections setting JSON by the caller), so inlining the IN list is
        // injection-safe. An empty set matches nothing.
        if let Some(ids) = f.collection_ids.as_deref() {
            if ids.is_empty() {
                conditions.push("1 = 0".to_string());
            } else {
                let list = ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                conditions.push(format!("t.album_id IN ({list})"));
            }
        }

        // Smart collection: the caller resolved its rules to concrete track ids
        // (our own i64s), inlined the same injection-safe way as album ids above.
        // An empty set matches nothing.
        if let Some(ids) = f.collection_track_ids.as_deref() {
            if ids.is_empty() {
                conditions.push("1 = 0".to_string());
            } else {
                let list = ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                conditions.push(format!("t.id IN ({list})"));
            }
        }

        // L'année d'enregistrement vit sur l'ALBUM : jointure par EXISTS.
        if let Some(c) = ph.in_list("alo.original_year", f.original_years.len()) {
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM albums alo WHERE alo.id = t.album_id AND {c})"
            ));
            for v in &f.original_years {
                owned_params.push(SqlValue::Int(*v));
            }
        }

        // Favoris du profil 1 : la piste elle-même, ou son album. Vocabulaire
        // FERMÉ — le SQL est un littéral, jamais l'entrée de la requête.
        if let Some(c) = any_of(
            f.favorites
                .iter()
                .filter_map(|k| favorite_condition(k))
                .map(str::to_string)
                .collect(),
        ) {
            conditions.push(c);
        }

        if let Some(c) = ph.in_list_ci("pl.name", f.playlists.len()) {
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM playlist_tracks pt JOIN playlists pl ON pl.id = pt.playlist_id \
                 WHERE pt.track_id = t.id AND {c})"
            ));
            for v in &f.playlists {
                owned_params.push(SqlValue::Text(v.clone()));
            }
        }

        // Étiquette manquante : liste FERMÉE, le SQL ne dépend jamais de
        // l'entrée brute. « Manquant » = NULL ou chaîne vide (un tag effacé
        // laisse souvent une chaîne vide, et l'utilisateur ne fait pas la
        // différence).
        if let Some(c) = any_of(
            f.untagged
                .iter()
                .filter_map(|k| untagged_condition(k).map(str::to_string))
                .collect(),
        ) {
            conditions.push(c);
        }

        if let Some(query) = f.q.as_deref().filter(|s| !s.is_empty()) {
            // ⚠️ DEUX marqueurs, DEUX valeurs liées — et non un marqueur réutilisé.
            //
            // Défaut PRÉEXISTANT trouvé en réécrivant cette fonction : le même
            // `{p}` était écrit deux fois pour une seule valeur empilée. En
            // PostgreSQL `$1` répété est légal et lie la même valeur ; en SQLite
            // chaque `?` anonyme consomme un indice, la requête en réclamait donc
            // deux et n'en recevait qu'un. `rusqlite` refuse le compte
            // (`InvalidParameterCount`), la requête échouait, et
            // `GET /library/tracks?q=…` rendait une liste VIDE avec un total à
            // zéro — sur SQLite, c'est-à-dire l'installation par défaut.
            //
            // Aucun appelant du client web ne passe `q` à cette route
            // aujourd'hui (Oxygen filtre sa fenêtre côté navigateur, la
            // recherche passe par `/library/search`), ce qui explique que
            // personne ne l'ait signalé.
            let like = format!("%{query}%");
            let p = ph.take();
            let p2 = ph.take();
            conditions.push(format!(
                "(LOWER(unaccent(t.title)) LIKE LOWER(unaccent({p})) OR LOWER(unaccent(ar.name)) LIKE LOWER(unaccent({p2})))"
            ));
            owned_params.push(SqlValue::Text(like.clone()));
            owned_params.push(SqlValue::Text(like));
        }

        // Albums masqués (#1391) : leurs pistes sortent de la vue filtrée,
        // TOUJOURS — ce prédicat n'est pas une facette (il n'entre pas dans
        // `is_active`), c'est le socle de la vue. Le compteur juste en
        // dessous partage `where_clause`, donc liste et total ne peuvent pas
        // diverger.
        conditions.push(hidden_tracks_excluded().to_string());

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        // Count total matching
        let count_sql = format!(
            "SELECT COUNT(*) FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id{}",
            where_clause
        );
        let refs: Vec<&dyn ToSqlValue> =
            owned_params.iter().map(|v| v as &dyn ToSqlValue).collect();
        let total = self
            .db
            .query_one(&count_sql, &refs)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);

        // Fetch paginated results
        let limit_ph = ph.take();
        let offset_ph = ph.take();
        let data_sql = format!(
            "{}{} ORDER BY LOWER(ar.name), LOWER(al.title), CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER) LIMIT {} OFFSET {}",
            sql::select_track(),
            where_clause,
            limit_ph,
            offset_ph
        );
        let mut all_params = owned_params.clone();
        all_params.push(SqlValue::Int(limit));
        all_params.push(SqlValue::Int(offset));
        let all_refs: Vec<&dyn ToSqlValue> =
            all_params.iter().map(|v| v as &dyn ToSqlValue).collect();
        let rows = self.db.query_many(&data_sql, &all_refs)?;
        Ok((rows.iter().map(row_to_track).collect(), total))
    }

    pub fn update_mtime_and_size(
        &self,
        file_path: &str,
        mtime: f64,
        file_size: i64,
    ) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_mtime_and_size, sql::update_mtime_and_size);
        let params: [&dyn ToSqlValue; 3] = [&mtime, &file_size, &file_path];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_audio_hash(&self, file_path: &str, audio_hash: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_audio_hash, sql::update_audio_hash);
        let params: [&dyn ToSqlValue; 2] = [&audio_hash, &file_path];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Persist a duration recovered at play time (see the orchestrator's
    /// play-time backfill) so a track scanned with `duration_ms = 0` self-heals.
    pub fn update_duration(&self, id: i64, duration_ms: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::update_duration, sql::update_duration);
        let params: [&dyn ToSqlValue; 2] = [&duration_ms, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn list_by_album(&self, album_id: i64) -> Result<Vec<Track>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_album, sql::list_by_album);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        let rows = self.db.query_many_strong(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    /// Hydrate tracks for a set of ids in ONE query. Order is not preserved
    /// (SQL `IN` is unordered) — the caller reorders (e.g. by acoustic-similarity
    /// rank). Ids are trusted i64 from our own queries, so inlining them is safe
    /// and avoids a variable-length placeholder list across dialects.
    pub fn list_by_ids(&self, ids: &[i64]) -> Result<Vec<Track>, TuneError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_list = ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("{} WHERE t.id IN ({id_list})", sql::select_track());
        let rows = self.db.query_many_strong(&sql, &[])?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    /// Like `list_by_album` but restricted to tracks matching an active
    /// quality/format filter, so the album detail agrees with a filtered grid.
    /// Sergio: a Hi-Res + 96kHz + FLAC filter matched a mixed album (the grid
    /// matches if ANY track qualifies), then the detail listed every track —
    /// including MP3/44.1 ones. The predicates mirror `AlbumRepo::list_filtered`
    /// (on the `t.` alias) so grid and detail stay consistent. With no filter it
    /// delegates to `list_by_album` (identical behavior).
    pub fn list_by_album_filtered(
        &self,
        album_id: i64,
        format: Option<&str>,
        quality: Option<&str>,
    ) -> Result<Vec<Track>, TuneError> {
        let mut extra = String::new();
        // Format is user-supplied: validate against an allowlist so it can be
        // inlined safely (no injection). The quality arms are constant strings.
        if let Some(fmt) = format {
            let f = fmt.to_lowercase();
            const ALLOWED: &[&str] = &[
                "flac", "mp3", "aac", "alac", "wav", "aiff", "aif", "dsf", "dff", "dsd", "ogg",
                "opus", "wma", "ape", "wv", "m4a",
            ];
            if ALLOWED.contains(&f.as_str()) {
                extra.push_str(&format!(" AND t.format = '{f}'"));
            }
        }
        match quality {
            Some("dsd") => extra.push_str(" AND t.format IN ('dsd','dsf','dff')"),
            Some("hires") => extra.push_str(" AND (t.sample_rate > 44100 OR t.bit_depth > 16)"),
            Some("cd") => extra.push_str(" AND t.sample_rate = 44100 AND t.bit_depth = 16"),
            Some("lossy") => extra.push_str(" AND t.format IN ('mp3','aac','ogg','opus','wma')"),
            _ => {}
        }
        if extra.is_empty() {
            return self.list_by_album(album_id);
        }
        let base = self.dialect_sql(sql::list_by_album, sql::list_by_album);
        let sql = base.replacen(" ORDER BY", &format!("{extra} ORDER BY"), 1);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        let rows = self.db.query_many_strong(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn list_by_artist(&self, artist_id: i64) -> Result<Vec<Track>, TuneError> {
        let sql = self.dialect_sql(sql::list_by_artist, sql::list_by_artist);
        let params: [&dyn ToSqlValue; 1] = [&artist_id];
        let rows = self.db.query_many_strong(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Track>, TuneError> {
        let fts_query = crate::db::engine::format_fts_query(self.db.engine(), query);
        let like = format!("%{query}%");
        let trimmed = query.trim();
        let sql = self.dialect_sql(sql::search, sql::search);
        let params: [&dyn ToSqlValue; 6] = [&fts_query, &like, &like, &like, &trimmed, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn find_by_path(&self, path: &str) -> Result<Option<Track>, TuneError> {
        let sql = self.dialect_sql(sql::get_by_path, sql::get_by_path);
        let params: [&dyn ToSqlValue; 1] = [&path];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_track))
    }

    /// `(file_path, album_artist)` of every track whose file path begins with
    /// `dir_prefix`. Used by the file-watcher to decide compilation status for a
    /// single re-imported file from its already-scanned siblings — a folder with
    /// 2+ distinct album_artists is a various-artists compilation (JP Borderies).
    /// The caller filters to direct children of the folder.
    pub fn siblings_album_artists(
        &self,
        dir_prefix: &str,
    ) -> Result<Vec<(String, Option<String>)>, TuneError> {
        let ph = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let esc = like_escape_clause(self.db.engine());
        let sql =
            format!("SELECT file_path, album_artist FROM tracks WHERE file_path LIKE {ph}{esc}");
        let like = format!("{dir_prefix}%");
        let params: [&dyn ToSqlValue; 1] = [&like];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|c| {
                let fp = c.first().and_then(|v| v.as_string())?;
                let aa = c.get(1).and_then(|v| v.as_string());
                Some((fp, aa))
            })
            .collect())
    }

    pub fn search_by_title(&self, title: &str, limit: i64) -> Result<Vec<Track>, TuneError> {
        let like = format!("%{title}%");
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "{} WHERE LOWER(unaccent(t.title)) LIKE LOWER(unaccent({})) LIMIT {}",
            sql::select_track(),
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&like, &limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn exists_by_audio_hash_and_album(
        &self,
        audio_hash: &str,
        album_id: i64,
    ) -> Result<bool, TuneError> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "SELECT COUNT(*) FROM tracks WHERE audio_hash = {} AND album_id = {}",
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&audio_hash, &album_id];
        let n = self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n > 0)
    }

    pub fn random_ids(&self, limit: i64) -> Result<Vec<i64>, TuneError> {
        // Both engines accept `ORDER BY RANDOM()` (SQLite) /
        // `ORDER BY random()` (PG). The lowercase form works on both.
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "SELECT id FROM tracks ORDER BY random() LIMIT {}",
            make_ph(1)
        );
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    pub fn count_doubtful(&self) -> Result<i64, TuneError> {
        let sql = format!(
            "SELECT COUNT(*) FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE (ar.name IS NULL OR ar.name = '' OR ar.name = 'Unknown Artist') \
                OR (t.duration_ms > 0 AND t.duration_ms < 5000) \
                OR (al.title IS NULL OR al.title = '')"
        );
        Ok(self
            .db
            .query_one(&sql, &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }

    pub fn list_doubtful(&self, limit: i64, offset: i64) -> Result<Vec<Track>, TuneError> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "{} \
             WHERE (ar.name IS NULL OR ar.name = '' OR ar.name = 'Unknown Artist') \
                OR (t.duration_ms > 0 AND t.duration_ms < 5000) \
                OR (al.title IS NULL OR al.title = '') \
             ORDER BY t.id LIMIT {} OFFSET {}",
            sql::select_track(),
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    /// Hydrate tracks for `ids`, **in the caller's order**, duplicates kept.
    ///
    /// Deux défauts corrigés (#2797) :
    ///
    /// 1. **Quadratique.** La réindexation faisait un
    ///    `tracks.iter().find(...)` par id demandé : O(n²) comparaisons sur
    ///    une grosse playlist. Elle passe par une `HashMap<i64, Track>`,
    ///    donc une seule passe, O(n).
    /// 2. **Limite de paramètres SQL.** Un placeholder par id faisait
    ///    échouer la requête au-delà de la limite du moteur — SQLite
    ///    `SQLITE_MAX_VARIABLE_NUMBER` (999 avant 3.32, 32766 depuis),
    ///    PostgreSQL 65535 paramètres par message Bind — et les routes
    ///    playlists rendaient alors une liste vide. Les ids sont des `i64`
    ///    issus de nos propres requêtes : ils sont **inlinés** (zéro
    ///    paramètre lié, même rationale que `list_by_ids`), ce qui met la
    ///    requête hors d'atteinte des deux limites, et découpés en lots pour
    ///    rester sous la limite de *longueur* d'instruction de SQLite (1 Mo
    ///    par défaut) : `ID_INLINE_BATCH` ids × 20 caractères au pire.
    ///
    /// Contrat inchangé : ordre du tableau d'entrée, doublons reproduits,
    /// ids absents simplement omis.
    pub fn get_multiple(&self, ids: &[i64]) -> Result<Vec<Track>, TuneError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // 1er temps : hydrater chaque id DISTINCT une seule fois, par lots.
        let mut wanted: Vec<i64> = ids.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let mut by_id: HashMap<i64, Track> = HashMap::with_capacity(wanted.len());
        for chunk in wanted.chunks(ID_INLINE_BATCH) {
            let id_list = chunk
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("{} WHERE t.id IN ({id_list})", sql::select_track());
            let rows = self.db.query_many(&sql, &[])?;
            for row in &rows {
                let track = row_to_track(row);
                if let Some(id) = track.id {
                    by_id.insert(id, track);
                }
            }
        }
        // 2e temps : réordonner en mémoire — un accès haché par id, O(n).
        Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
    }

    // ─── Group B/C: write_tx + simple inline ──────────────────────

    /// Insert multiple tracks using individual execute calls.
    ///
    /// **Important**: this method does NOT start its own transaction.
    /// The caller is responsible for wrapping the call in a transaction
    /// (e.g. `BEGIN IMMEDIATE` / `COMMIT`) if atomicity is needed.
    /// Using `write_tx` here would fail with "cannot start a transaction
    /// within a transaction" when the caller already holds one.
    pub fn create_batch(&self, tracks: &[Track]) -> Result<usize, TuneError> {
        let insert_sql = self.dialect_sql(sql::insert, sql::insert);
        let mut count = 0usize;
        let mut row_params: Vec<Vec<SqlValue>> = Vec::with_capacity(tracks.len());
        for track in tracks {
            if track.title.to_lowercase().contains("personal jesus") {
                tracing::warn!(
                    title = %track.title,
                    album_id = ?track.album_id,
                    artist_id = ?track.artist_id,
                    album_artist = ?track.album_artist,
                    file = ?track.file_path,
                    "BUG_TRACE_insert_personal_jesus"
                );
            }
            let params: [&dyn ToSqlValue; 28] = [
                &track.title,
                &track.album_id,
                &track.artist_id,
                &track.album_artist,
                &track.disc_number,
                &track.disc_subtitle,
                &track.track_number,
                &track.duration_ms,
                &track.file_path,
                &track.format,
                &track.sample_rate,
                &track.bit_depth,
                &track.channels,
                &track.file_mtime,
                &track.file_size,
                &track.audio_hash,
                &track.source,
                &track.source_id,
                &track.isrc,
                &track.genre,
                &track.genres,
                &track.composer,
                &track.year,
                &track.bpm,
                &track.label,
                &track.musicbrainz_recording_id,
                &track.comments,
                &track.cover_path,
            ];
            row_params.push(params.iter().map(|p| p.to_sql_value()).collect());
        }
        // One backend call for the whole batch: on Postgres this reuses a
        // single connection + prepared statement instead of a per-row
        // runtime hop (see DbBackend::execute_many).
        for (track, res) in tracks
            .iter()
            .zip(self.db.execute_many(&insert_sql, &row_params))
        {
            match res {
                Ok(_) => count += 1,
                // Previously this failure was swallowed silently: the scanner
                // reported "files=N errors=0" while the tracks never landed in
                // the library (JP Borderies: ~205 tracks in DB vs ~779 on disk
                // after a delete + full rescan). Log it so the drop is visible
                // and the root cause (stale album_id/artist_id FK from an
                // importer cache surviving a batch rollback) is diagnosable.
                Err(e) => tracing::warn!(
                    file = ?track.file_path,
                    album_id = ?track.album_id,
                    artist_id = ?track.artist_id,
                    error = %e,
                    "track_insert_failed_in_batch"
                ),
            }
        }
        Ok(count)
    }

    /// Update multiple tracks using individual execute calls.
    ///
    /// **Important**: this method does NOT start its own transaction.
    /// The caller is responsible for wrapping the call in a transaction
    /// (e.g. `BEGIN IMMEDIATE` / `COMMIT`) if atomicity is needed.
    /// See `create_batch` for rationale.
    pub fn update_batch(&self, tracks: &[Track]) -> Result<usize, TuneError> {
        let update_sql = self.dialect_sql(sql::update, sql::update);
        let mut count = 0usize;
        // Rows without an id are skipped, so collect the params first and
        // batch them through one execute_many call (see create_batch).
        let mut row_params: Vec<Vec<SqlValue>> = Vec::with_capacity(tracks.len());
        for track in tracks {
            let Some(id) = track.id else { continue };
            let params: [&dyn ToSqlValue; 25] = [
                &track.title,
                &track.album_id,
                &track.artist_id,
                &track.album_artist,
                &track.disc_number,
                &track.disc_subtitle,
                &track.track_number,
                &track.duration_ms,
                &track.file_path,
                &track.format,
                &track.sample_rate,
                &track.bit_depth,
                &track.channels,
                &track.file_mtime,
                &track.file_size,
                &track.audio_hash,
                &track.genre,
                &track.genres,
                &track.composer,
                &track.year,
                &track.bpm,
                &track.label,
                &track.musicbrainz_recording_id,
                &track.comments,
                &id,
            ];
            row_params.push(params.iter().map(|p| p.to_sql_value()).collect());
        }
        for res in self.db.execute_many(&update_sql, &row_params) {
            match res {
                Ok(_) => count += 1,
                Err(e) => tracing::warn!(error = %e, "track_update_failed_in_batch"),
            }
        }
        Ok(count)
    }

    // ─── Group B: metadata accessors via DbBackend ───────────────────
    // Backed by migration `003_track_metadata_columns.sql` on PG.

    pub fn get_synced_lyrics(&self, track_id: i64) -> Result<Option<String>, TuneError> {
        let sql = self.dialect_sql(sql::get_synced_lyrics, sql::get_synced_lyrics);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_string())))
    }

    pub fn set_synced_lyrics(&self, track_id: i64, json: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::set_synced_lyrics, sql::set_synced_lyrics);
        let params: [&dyn ToSqlValue; 2] = [&json, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_trailing_silence(&self, track_id: i64) -> Result<Option<i64>, TuneError> {
        let sql = self.dialect_sql(sql::get_trailing_silence, sql::get_trailing_silence);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64())))
    }

    pub fn set_trailing_silence(&self, track_id: i64, ms: i64) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::set_trailing_silence, sql::set_trailing_silence);
        let params: [&dyn ToSqlValue; 2] = [&ms, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn set_acoustid(
        &self,
        track_id: i64,
        fingerprint: &str,
        confidence: f64,
    ) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::set_acoustid, sql::set_acoustid);
        let params: [&dyn ToSqlValue; 3] = [&fingerprint, &confidence, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn list_unidentified(&self, limit: i64) -> Result<Vec<Track>, TuneError> {
        let sql = self.dialect_sql(sql::list_unidentified, sql::list_unidentified);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn get_waveform(&self, track_id: i64) -> Result<Option<String>, TuneError> {
        let sql = self.dialect_sql(sql::get_waveform, sql::get_waveform);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_string())))
    }

    pub fn set_waveform(&self, track_id: i64, json: &str) -> Result<(), TuneError> {
        let sql = self.dialect_sql(sql::set_waveform, sql::set_waveform);
        let params: [&dyn ToSqlValue; 2] = [&json, &track_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_credits(
        &self,
        track_id: i64,
    ) -> Result<Vec<crate::db::models::TrackCredit>, TuneError> {
        let sql = self.dialect_sql(sql::get_credits, sql::get_credits);
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| crate::db::models::TrackCredit {
                id: cols.first().and_then(|v| v.as_i64()),
                track_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                artist_id: cols.get(2).and_then(|v| v.as_i64()),
                artist_name: cols.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                role: cols.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                instrument: cols.get(5).and_then(|v| v.as_string()),
                position: cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            })
            .collect())
    }

    pub fn get_all_paths(&self) -> Result<HashSet<String>, TuneError> {
        let rows = self.db.query_many(sql::get_all_paths(), &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
            .collect())
    }

    #[allow(clippy::type_complexity)]
    pub fn get_all_local_file_info(
        &self,
    ) -> Result<HashMap<String, (i64, Option<f64>, Option<i64>)>, TuneError> {
        let rows = self.db.query_many(sql::get_all_local_file_info(), &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| {
                let id = cols.first().and_then(|v| v.as_i64())?;
                let path = cols.get(1).and_then(|v| v.as_string())?;
                let mtime = cols.get(2).and_then(|v| v.as_f64());
                let size = cols.get(3).and_then(|v| v.as_i64());
                Some((path, (id, mtime, size)))
            })
            .collect())
    }

    /// List all local tracks (with file_path set). Used by rescan-metadata to
    /// re-read tags from disk without doing a full library scan.
    pub fn list_all_local(&self) -> Result<Vec<Track>, TuneError> {
        let sql = format!(
            "{} WHERE t.file_path IS NOT NULL AND t.file_path != ''",
            sql::select_track()
        );
        let rows = self.db.query_many(&sql, &[])?;
        Ok(rows.iter().map(row_to_track).collect())
    }

    pub fn get_existing_audio_hash_album_pairs(&self) -> Result<HashSet<(String, i64)>, TuneError> {
        let rows = self
            .db
            .query_many(sql::get_existing_audio_hash_album_pairs(), &[])?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| {
                let hash = cols.first().and_then(|v| v.as_string())?;
                let album_id = cols.get(1).and_then(|v| v.as_i64())?;
                Some((hash, album_id))
            })
            .collect())
    }

    pub fn get_existing_audio_hash_album_paths(
        &self,
    ) -> Result<HashMap<(String, i64), Vec<String>>, TuneError> {
        let rows = self
            .db
            .query_many(sql::get_existing_audio_hash_album_paths(), &[])?;
        let mut paths: HashMap<(String, i64), Vec<String>> = HashMap::new();
        for cols in rows {
            let Some(hash) = cols.first().and_then(|v| v.as_string()) else {
                continue;
            };
            let Some(album_id) = cols.get(1).and_then(|v| v.as_i64()) else {
                continue;
            };
            let Some(path) = cols.get(2).and_then(|v| v.as_string()) else {
                continue;
            };
            paths.entry((hash, album_id)).or_default().push(path);
        }
        Ok(paths)
    }

    pub fn paths_by_audio_hash_and_album(
        &self,
        audio_hash: &str,
        album_id: i64,
    ) -> Result<Vec<String>, TuneError> {
        let make_ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let sql = format!(
            "SELECT file_path FROM tracks WHERE source = 'local' \
             AND audio_hash = {} AND album_id = {} AND file_path IS NOT NULL",
            make_ph(1),
            make_ph(2)
        );
        let params: [&dyn ToSqlValue; 2] = [&audio_hash, &album_id];
        Ok(self
            .db
            .query_many(&sql, &params)?
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
            .collect())
    }

    pub fn deduplicate(&self) -> Result<i64, TuneError> {
        let rows = self.db.query_many(
            "SELECT id, audio_hash, file_path FROM tracks \
             WHERE source = 'local' AND audio_hash IS NOT NULL AND file_path IS NOT NULL \
             ORDER BY audio_hash, id",
            &[],
        )?;
        let mut candidates: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        for cols in rows {
            let Some(id) = cols.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let Some(hash) = cols.get(1).and_then(|v| v.as_string()) else {
                continue;
            };
            let Some(path) = cols.get(2).and_then(|v| v.as_string()) else {
                continue;
            };
            candidates.entry(hash).or_default().push((id, path));
        }

        let mut delete_ids = Vec::new();
        for group in candidates.into_values().filter(|group| group.len() > 1) {
            let mut representatives: Vec<(i64, String)> = Vec::new();
            for candidate in group {
                let is_exact_duplicate = representatives.iter().any(|(_, representative)| {
                    crate::scanner::hasher::files_are_byte_identical(
                        std::path::Path::new(&candidate.1),
                        std::path::Path::new(representative),
                    )
                    .unwrap_or(false)
                });
                if is_exact_duplicate {
                    delete_ids.push(candidate.0);
                } else {
                    representatives.push(candidate);
                }
            }
        }

        if delete_ids.is_empty() {
            return Ok(0);
        }

        let delete_sql = match self.db.engine() {
            Engine::Sqlite => "DELETE FROM tracks WHERE id = ?".to_string(),
            Engine::Postgres => "DELETE FROM tracks WHERE id = $1".to_string(),
        };
        self.db.write_tx(&mut |tx| {
            for id in &delete_ids {
                tx.execute(&delete_sql, &[id])?;
            }
            Ok(())
        })?;
        Ok(delete_ids.len() as i64)
    }
}

fn row_to_track(cols: &Vec<SqlValue>) -> Track {
    Track {
        id: cols.first().and_then(|v| v.as_i64()),
        title: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        album_id: cols.get(2).and_then(|v| v.as_i64()),
        album_title: cols.get(3).and_then(|v| v.as_string()),
        artist_id: cols.get(4).and_then(|v| v.as_i64()),
        artist_name: cols.get(5).and_then(|v| v.as_string()),
        album_artist: cols.get(6).and_then(|v| v.as_string()),
        disc_number: cols.get(7).and_then(|v| v.as_i64()).unwrap_or(1) as i32,
        disc_subtitle: cols.get(8).and_then(|v| v.as_string()),
        track_number: cols.get(9).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        duration_ms: cols.get(10).and_then(|v| v.as_i64()).unwrap_or(0),
        file_path: cols.get(11).and_then(|v| v.as_string()),
        format: cols.get(12).and_then(|v| v.as_string()),
        sample_rate: cols.get(13).and_then(|v| v.as_i64()).map(|n| n as i32),
        bit_depth: cols.get(14).and_then(|v| v.as_i64()).map(|n| n as i32),
        channels: cols.get(15).and_then(|v| v.as_i64()).unwrap_or(2) as i32,
        file_mtime: cols.get(16).and_then(|v| v.as_f64()),
        file_size: cols.get(17).and_then(|v| v.as_i64()),
        audio_hash: cols.get(18).and_then(|v| v.as_string()),
        source: cols
            .get(19)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "local".into()),
        source_id: cols.get(20).and_then(|v| v.as_string()),
        isrc: cols.get(21).and_then(|v| v.as_string()),
        genre: cols.get(22).and_then(|v| v.as_string()),
        composer: cols.get(23).and_then(|v| v.as_string()),
        year: cols.get(24).and_then(|v| v.as_i64()).map(|n| n as i32),
        bpm: cols.get(25).and_then(|v| v.as_f64()),
        label: cols.get(26).and_then(|v| v.as_string()),
        musicbrainz_recording_id: cols.get(27).and_then(|v| v.as_string()),
        cover_path: cols.get(28).and_then(|v| v.as_string()),
        genres: cols.get(29).and_then(|v| v.as_string()),
        comments: cols.get(30).and_then(|v| v.as_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    /// #1391 — les pistes d'un album masqué sortent de la vue pistes (les
    /// deux chemins de `GET /library/tracks`), de la recherche et de la vue
    /// artiste ; une piste SANS album reste visible ; `list_by_album` reste
    /// ENTIER pour que l'album masqué demeure jouable.
    #[test]
    fn les_pistes_d_un_album_masque_sortent_des_vues() {
        let db = test_db();
        let artist_id = ArtistRepo::new(db.clone())
            .create(&Artist::new("Massive Attack".into()))
            .unwrap();
        let albums = AlbumRepo::new(db.clone());
        let album_id = albums
            .get_or_create("Mezzanine", artist_id, None)
            .unwrap()
            .id
            .unwrap();
        let repo = TrackRepo::new(db.clone());

        let mut cachee = Track::new("Teardrop".into());
        cachee.album_id = Some(album_id);
        cachee.artist_id = Some(artist_id);
        cachee.file_path = Some("/music/Mezzanine/teardrop.flac".into());
        let cachee_id = repo.create(&cachee).unwrap();

        // Une piste sans album : le filtre ne doit PAS l'avaler (piège du
        // `t.album_id` NULL).
        let mut libre = Track::new("Sans album".into());
        libre.artist_id = Some(artist_id);
        libre.file_path = Some("/music/loose.flac".into());
        let libre_id = repo.create(&libre).unwrap();

        crate::db::hidden_repo::HiddenRepo::new(db.clone())
            .hide_album(album_id)
            .unwrap();

        // Chemin non facetté.
        let visibles = repo.list_visible(100, 0).unwrap();
        assert_eq!(
            visibles.iter().filter_map(|t| t.id).collect::<Vec<_>>(),
            vec![libre_id],
            "seule la piste sans album reste visible"
        );
        assert_eq!(repo.count_visible().unwrap(), 1);
        assert_eq!(repo.count().unwrap(), 2, "le compte COMPLET reste entier");

        // Chemin facetté : même exclusion, et le total suit la liste.
        let filtre = crate::db::facet_filter::TrackFilter {
            artists: vec!["Massive Attack".into()],
            ..Default::default()
        };
        let (pistes, total) = repo.list_filtered(&filtre, 100, 0).unwrap();
        assert!(pistes.iter().all(|t| t.id != Some(cachee_id)));
        assert_eq!(total, pistes.len() as i64);

        // Recherche et vue artiste.
        assert!(
            repo.search("Teardrop", 10).unwrap().is_empty(),
            "la recherche ne doit pas trahir la piste masquée"
        );
        assert!(
            repo.list_by_artist(artist_id)
                .unwrap()
                .iter()
                .all(|t| t.id != Some(cachee_id))
        );

        // Masqué n'est pas supprimé : l'album se joue toujours.
        assert_eq!(repo.list_by_album(album_id).unwrap().len(), 1);
    }

    /// Forum #1312. A track filed under a folder-named album must be able to
    /// carry its own artwork, and a track without one must still show its
    /// album's — the fallback is what keeps every normal album unchanged.
    #[test]
    fn track_cover_overrides_album_cover_and_falls_back_to_it() {
        let db = test_db();
        let artist_id = ArtistRepo::new(db.clone())
            .create(&Artist::new("Various Artists".into()))
            .unwrap();
        let albums = AlbumRepo::new(db.clone());
        let album_id = albums
            .get_or_create("Audio Formats", artist_id, None)
            .unwrap()
            .id
            .unwrap();
        albums
            .update_cover_path(album_id, "album-sleeve-hash")
            .unwrap();

        let repo = TrackRepo::new(db.clone());

        // The file that lent its artwork to the whole folder before #1312.
        let mut owned = Track::new("Les grands restaurants".into());
        owned.album_id = Some(album_id);
        owned.artist_id = Some(artist_id);
        owned.file_path = Some("/music/Audio Formats/alliye.flac".into());
        owned.cover_path = Some("its-own-hash".into());
        let owned_id = repo.create(&owned).unwrap();

        // A file in the same folder with no embedded artwork.
        let mut bare = Track::new("Take five".into());
        bare.album_id = Some(album_id);
        bare.artist_id = Some(artist_id);
        bare.file_path = Some("/music/Audio Formats/jarreau.wav".into());
        let bare_id = repo.create(&bare).unwrap();

        assert_eq!(
            repo.get(owned_id).unwrap().unwrap().cover_path.as_deref(),
            Some("its-own-hash"),
            "a track with its own cover must not show the album's"
        );
        assert_eq!(
            repo.get(bare_id).unwrap().unwrap().cover_path.as_deref(),
            Some("album-sleeve-hash"),
            "a track without its own cover must fall back to the album's"
        );
    }

    #[test]
    fn dedup_display_collapses_content_duplicates() {
        let dir = tempfile::TempDir::new().unwrap();
        let original_path = dir.path().join("time.flac");
        let copy_path = dir.path().join("time-copy.flac");
        let copy2_path = dir.path().join("time-copy-2.flac");
        let bytes = vec![0x44u8; 128 * 1024];
        std::fs::write(&original_path, &bytes).unwrap();
        std::fs::write(&copy_path, &bytes).unwrap();
        std::fs::write(&copy2_path, &bytes).unwrap();

        // Same album, same hash → the copy is hidden, first kept.
        let mut a = Track::new("Time".into());
        a.album_id = Some(1);
        a.disc_number = 1;
        a.track_number = 4;
        a.audio_hash = Some("HASH_TIME".into());
        a.file_path = Some(original_path.to_string_lossy().into_owned());
        let mut a_copy = a.clone();
        a_copy.file_path = Some(copy_path.to_string_lossy().into_owned());
        let mut a_copy2 = a.clone();
        a_copy2.file_path = Some(copy2_path.to_string_lossy().into_owned());

        // Same album, hash-less: dedup falls back to disc/track/title.
        let mut b = Track::new("Money".into());
        b.album_id = Some(1);
        b.disc_number = 1;
        b.track_number = 6;
        let mut b_copy = b.clone();
        b_copy.title = "MONEY".into(); // case-insensitive match

        // Genuinely different track in the same album is kept.
        let mut c = Track::new("Us and Them".into());
        c.album_id = Some(1);
        c.disc_number = 1;
        c.track_number = 7;

        // Same recording on a DIFFERENT album (compilation) must NOT collapse.
        let mut a_other_album = a.clone();
        a_other_album.album_id = Some(2);

        let out = dedup_display_tracks(vec![
            a.clone(),
            a_copy,
            a_copy2,
            b.clone(),
            b_copy,
            c.clone(),
            a_other_album,
        ]);

        let titles: Vec<(&str, Option<i64>)> =
            out.iter().map(|t| (t.title.as_str(), t.album_id)).collect();
        assert_eq!(
            titles,
            vec![
                ("Time", Some(1)),        // first copy kept
                ("Money", Some(1)),       // first kept, "MONEY" dropped
                ("Us and Them", Some(1)), // distinct track survives
                ("Time", Some(2)),        // same song, other album: kept
            ]
        );
        // The retained "Time" is the first-seen path (album 1), not a copy.
        assert_eq!(out[0].file_path.as_deref(), a.file_path.as_deref());
    }

    #[test]
    fn dedup_display_garde_deux_fichiers_distincts_au_meme_hash_candidat() {
        let dir = tempfile::TempDir::new().unwrap();
        let left_path = dir.path().join("left.flac");
        let right_path = dir.path().join("right.flac");
        let sample_size = 65_536;
        let mut left = vec![0u8; sample_size * 4];
        let mut right = vec![0u8; sample_size * 4];
        left[sample_size * 2..].fill(0x11);
        right[sample_size * 2..].fill(0x22);
        std::fs::write(&left_path, left).unwrap();
        std::fs::write(&right_path, right).unwrap();
        let hash = crate::scanner::hasher::compute_audio_hash(&left_path).unwrap();
        assert_eq!(
            Some(hash.clone()),
            crate::scanner::hasher::compute_audio_hash(&right_path)
        );

        let mut left_track = Track::new("Left".into());
        left_track.album_id = Some(1);
        left_track.audio_hash = Some(hash.clone());
        left_track.file_path = Some(left_path.to_string_lossy().into_owned());
        let mut right_track = Track::new("Right".into());
        right_track.album_id = Some(1);
        right_track.audio_hash = Some(hash);
        right_track.file_path = Some(right_path.to_string_lossy().into_owned());

        let out = dedup_display_tracks(vec![left_track, right_track]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn update_duration_backfills_a_zero_duration_track() {
        // A track scanned with duration_ms = 0 (scan timeout / unreadable DSD)
        // is what the play-time backfill repairs. Verify the persist path.
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut track = Track::new("Silent Length".into());
        track.file_path = Some("/music/mystery.dsf".into());
        track.duration_ms = 0;
        let id = repo.create(&track).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().duration_ms, 0);

        repo.update_duration(id, 207_000).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().duration_ms, 207_000);
    }

    #[test]
    fn crud_track() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let aid = artist_repo
            .create(&Artist::new("Pink Floyd".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("DSOTM", aid, Some(1973))
            .unwrap()
            .id
            .unwrap();

        let mut track = Track::new("Time".into());
        track.artist_id = Some(aid);
        track.album_id = Some(alid);
        track.file_path = Some("/music/pink_floyd/dsotm/time.flac".into());
        track.duration_ms = 413000;
        track.sample_rate = Some(44100);
        track.bit_depth = Some(16);

        let id = repo.create(&track).unwrap();
        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.title, "Time");
        assert_eq!(fetched.artist_name.as_deref(), Some("Pink Floyd"));
        assert_eq!(fetched.album_title.as_deref(), Some("DSOTM"));

        let by_path = repo
            .get_by_path("/music/pink_floyd/dsotm/time.flac")
            .unwrap();
        assert!(by_path.is_some());

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    // Bilou / Oxygen "by genre": a compilation whose files carry NO ALBUMARTIST
    // tag must still report the album's canonical artist ("Various Artists"),
    // not the per-track guest artist — otherwise the client-side album grouping
    // over a genre-filtered subset shows track 1's artist.
    #[test]
    fn album_artist_falls_back_to_album_canonical_when_tag_missing() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let va = artist_repo
            .create(&Artist::new("Various Artists".into()))
            .unwrap();
        let guest = artist_repo
            .create(&Artist::new("Guest One".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("Comp", va, None)
            .unwrap()
            .id
            .unwrap();

        let mut track = Track::new("Song".into());
        track.album_id = Some(alid);
        track.artist_id = Some(guest);
        track.album_artist = None; // file has no ALBUMARTIST tag
        let id = repo.create(&track).unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.artist_name.as_deref(), Some("Guest One"));
        assert_eq!(fetched.album_artist.as_deref(), Some("Various Artists"));
    }

    // Regression: a real per-file ALBUMARTIST tag still wins over the fallback.
    #[test]
    fn album_artist_tag_wins_over_album_canonical() {
        let db = test_db();
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let repo = TrackRepo::new(db);

        let aid = artist_repo.create(&Artist::new("The Band".into())).unwrap();
        let alid = album_repo
            .get_or_create("LP", aid, None)
            .unwrap()
            .id
            .unwrap();

        let mut track = Track::new("Tune".into());
        track.album_id = Some(alid);
        track.artist_id = Some(aid);
        track.album_artist = Some("Tagged Albumartist".into());
        let id = repo.create(&track).unwrap();

        assert_eq!(
            repo.get(id).unwrap().unwrap().album_artist.as_deref(),
            Some("Tagged Albumartist")
        );
    }

    #[test]
    fn list_by_ids_hydrates_requested_tracks_only() {
        let db = test_db();
        let repo = TrackRepo::new(db);
        let mut a = Track::new("A".into());
        a.file_path = Some("/a.flac".into());
        let mut b = Track::new("B".into());
        b.file_path = Some("/b.flac".into());
        let mut c = Track::new("C".into());
        c.file_path = Some("/c.flac".into());
        let ia = repo.create(&a).unwrap();
        let _ib = repo.create(&b).unwrap();
        let ic = repo.create(&c).unwrap();

        let mut titles: Vec<String> = repo
            .list_by_ids(&[ia, ic])
            .unwrap()
            .iter()
            .map(|t| t.title.clone())
            .collect();
        titles.sort();
        assert_eq!(titles, vec!["A".to_string(), "C".to_string()]);
        assert!(repo.list_by_ids(&[]).unwrap().is_empty());
    }

    #[test]
    fn get_all_paths() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Song 1".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("Song 2".into());
        t2.file_path = Some("/b.flac".into());

        repo.create(&t1).unwrap();
        repo.create(&t2).unwrap();

        let paths = repo.get_all_paths().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("/a.flac"));
    }

    #[test]
    fn get_multiple_preserves_order() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t1 = Track::new("Alpha".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = Track::new("Beta".into());
        t2.file_path = Some("/2.flac".into());
        let mut t3 = Track::new("Gamma".into());
        t3.file_path = Some("/3.flac".into());

        let id1 = repo.create(&t1).unwrap();
        let id2 = repo.create(&t2).unwrap();
        let id3 = repo.create(&t3).unwrap();

        let result = repo.get_multiple(&[id3, id1, id2]).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].title, "Gamma");
        assert_eq!(result[1].title, "Alpha");
        assert_eq!(result[2].title, "Beta");
    }

    /// Sème `n` pistes d'ids 1..=n en quelques `execute_batch`, sans passer par
    /// `create` (une transaction par piste serait le coût dominant du test).
    fn seed_pistes(db: &SqliteDb, n: usize) {
        let mut sql = String::with_capacity(1 << 22);
        sql.push_str("BEGIN;\n");
        for id in 1..=n {
            sql.push_str(&format!(
                "INSERT INTO tracks (id, title, file_path, duration_ms) \
                 VALUES ({id}, 'piste {id}', '/musique/{id}.flac', {id});\n"
            ));
            if sql.len() > (1 << 22) {
                sql.push_str("COMMIT;\n");
                db.execute_batch(&sql).unwrap();
                sql.clear();
                sql.push_str("BEGIN;\n");
            }
        }
        sql.push_str("COMMIT;\n");
        db.execute_batch(&sql).unwrap();
    }

    /// #2797, défaut n°2 — la limite de paramètres SQL.
    ///
    /// L'ancienne forme posait UN placeholder par id. Au-delà de la limite du
    /// moteur (SQLite `SQLITE_MAX_VARIABLE_NUMBER` : 999 avant 3.32, 32766
    /// depuis ; PostgreSQL : 65535), la requête est refusée et les routes
    /// playlists rendaient une liste VIDE. 40 000 ids dépassent les deux
    /// seuils SQLite, quelle que soit la version liée.
    ///
    /// Contre-épreuve : en réinjectant la forme à placeholders, ce test
    /// échoue avec « too many SQL variables ».
    #[test]
    fn get_multiple_tient_au_dela_de_la_limite_de_parametres_2797() {
        let db = test_db();
        seed_pistes(&db, 6);
        let repo = TrackRepo::new(db);

        // 40 000 ids demandés, dont 6 seulement existent, disséminés.
        let mut ids: Vec<i64> = (1_000_000..1_040_000).collect();
        ids[0] = 4;
        ids[9_999] = 1;
        ids[19_999] = 6;
        ids[29_999] = 3;
        ids[39_998] = 5;
        ids[39_999] = 2;
        assert!(ids.len() > 32_766, "le test doit dépasser les deux seuils");

        let result = repo.get_multiple(&ids).expect("aucune erreur de moteur");
        let titles: Vec<&str> = result.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "piste 4", "piste 1", "piste 6", "piste 3", "piste 5", "piste 2"
            ],
            "résultat complet, dans l'ordre demandé, ids absents omis"
        );
    }

    /// #2797 — le contrat de sortie : ordre d'entrée, doublons reproduits,
    /// ids absents omis. La réindexation par `HashMap` ne doit rien changer
    /// à ce qu'observaient les appelants (positions de playlist, rang
    /// acoustique de `library/search`).
    #[test]
    fn get_multiple_reproduit_les_doublons_et_omet_les_absents_2797() {
        let db = test_db();
        seed_pistes(&db, 3);
        let repo = TrackRepo::new(db);

        let result = repo.get_multiple(&[3, 1, 999, 1, 2, 3, 1]).unwrap();
        let titles: Vec<&str> = result.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "piste 3", "piste 1", "piste 1", "piste 2", "piste 3", "piste 1"
            ]
        );
        assert!(repo.get_multiple(&[]).unwrap().is_empty());
    }

    /// #2797, défaut n°1 — la réindexation était quadratique
    /// (`tracks.iter().find(...)` par id demandé).
    ///
    /// Contre-épreuve de complexité : on mesure le même appel à `n` puis à
    /// `4n`. Un coût linéaire multiplie le temps par ~4 ; un coût quadratique
    /// par ~16. Le seuil est à 8× — à mi-chemin en échelle log, donc ~2×
    /// de marge de chaque côté pour ne pas devenir instable sur une machine
    /// de CI chargée. Meilleure de 3 passes, pour la même raison.
    #[test]
    fn get_multiple_ne_coute_pas_de_maniere_quadratique_2797() {
        use std::time::Instant;

        const N: usize = 4_000;
        let db = test_db();
        seed_pistes(&db, 4 * N);
        let repo = TrackRepo::new(db);

        let petit: Vec<i64> = (1..=N as i64).collect();
        let grand: Vec<i64> = (1..=(4 * N) as i64).collect();

        let mesure = |ids: &[i64]| {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                let out = repo.get_multiple(ids).unwrap();
                let dt = t0.elapsed().as_secs_f64();
                assert_eq!(out.len(), ids.len());
                best = best.min(dt);
            }
            best
        };

        let t_petit = mesure(&petit);
        let t_grand = mesure(&grand);
        let ratio = t_grand / t_petit.max(1e-6);
        assert!(
            ratio < 8.0,
            "coût quadratique : n={N} {:.1} ms, 4n={} {:.1} ms → ×{ratio:.1} (linéaire ≈ ×4)",
            t_petit * 1e3,
            4 * N,
            t_grand * 1e3,
        );
    }

    /// Mesure de référence pour #2797 : le coût par piste doit rester plat.
    /// `--ignored`, hors CI (build release, plusieurs dizaines de milliers de
    /// lignes semées) — c'est la table de preuve de la PR, pas un garde-fou.
    #[test]
    #[ignore]
    fn bench_get_multiple_2797() {
        use std::time::Instant;

        let db = test_db();
        seed_pistes(&db, 100_000);
        let repo = TrackRepo::new(db);
        for n in [1_000usize, 5_000, 20_000, 50_000, 100_000] {
            let ids: Vec<i64> = (1..=n as i64).collect();
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                let out = repo.get_multiple(&ids).unwrap();
                assert_eq!(out.len(), n);
                best = best.min(t0.elapsed().as_secs_f64());
            }
            eprintln!(
                "BENCH2797 n={n:>7} total={:>9.1} ms par_piste={:>7.3} µs",
                best * 1e3,
                best * 1e6 / n as f64
            );
        }
    }

    #[test]
    fn search_tracks() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        let mut t = Track::new("Comfortably Numb".into());
        t.file_path = Some("/numb.flac".into());
        repo.create(&t).unwrap();

        let results = repo.search("comfort", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn track_count() {
        let db = test_db();
        let repo = TrackRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        let mut t = Track::new("A".into());
        t.file_path = Some("/a.flac".into());
        repo.create(&t).unwrap();
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn cleanup_ne_supprime_pas_une_collision_de_hash_partiel() {
        let dir = tempfile::TempDir::new().unwrap();
        let left_path = dir.path().join("left.flac");
        let right_path = dir.path().join("right.flac");
        let sample_size = 65_536;
        let mut left = vec![0u8; sample_size * 4];
        let mut right = vec![0u8; sample_size * 4];
        left[sample_size * 2..].fill(0x31);
        right[sample_size * 2..].fill(0x42);
        std::fs::write(&left_path, left).unwrap();
        std::fs::write(&right_path, right).unwrap();
        let hash = crate::scanner::hasher::compute_audio_hash(&left_path).unwrap();
        assert_eq!(
            Some(hash.clone()),
            crate::scanner::hasher::compute_audio_hash(&right_path)
        );

        let db = test_db();
        let repo = TrackRepo::new(db);
        for (title, path) in [("Left", &left_path), ("Right", &right_path)] {
            let mut track = Track::new(title.into());
            track.file_path = Some(path.to_string_lossy().into_owned());
            track.audio_hash = Some(hash.clone());
            repo.create(&track).unwrap();
        }

        assert_eq!(repo.deduplicate().unwrap(), 0);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn cleanup_supprime_uniquement_la_copie_octet_pour_octet() {
        let dir = tempfile::TempDir::new().unwrap();
        let left_path = dir.path().join("left.flac");
        let right_path = dir.path().join("right.flac");
        let bytes = vec![0x5au8; 65_536 * 3];
        std::fs::write(&left_path, &bytes).unwrap();
        std::fs::write(&right_path, &bytes).unwrap();
        let hash = crate::scanner::hasher::compute_audio_hash(&left_path).unwrap();

        let db = test_db();
        let repo = TrackRepo::new(db);
        for (title, path) in [("Left", &left_path), ("Right", &right_path)] {
            let mut track = Track::new(title.into());
            track.file_path = Some(path.to_string_lossy().into_owned());
            track.audio_hash = Some(hash.clone());
            repo.create(&track).unwrap();
        }

        assert_eq!(repo.deduplicate().unwrap(), 1);
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn with_backend_constructor_full() {
        // All methods now go through DbBackend — no more sqlite_legacy
        // fallback. The with_backend path is fully functional on SQLite.
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = TrackRepo::with_backend(backend);
        let mut t = Track::new("X".into());
        t.file_path = Some("/x.flac".into());
        let id = repo.create(&t).unwrap();
        assert!(repo.get(id).unwrap().is_some());
        // Methods previously requiring sqlite_legacy now work via
        // DbBackend. get_all_paths reads from the base schema.
        assert!(repo.get_all_paths().unwrap().contains("/x.flac"));
        assert!(
            repo.get_existing_audio_hash_album_pairs()
                .unwrap()
                .is_empty()
        );
    }
}
