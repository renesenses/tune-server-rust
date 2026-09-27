//! One-shot SQLite → PostgreSQL data migration.
//!
//! This module reads all rows from the current SQLite `state.db` and
//! copies them into a PostgreSQL database at the provided URL. It is
//! designed as a migration tool, not a live engine switch — Tune
//! continues to run on SQLite after the migration completes.
//!
//! Gated behind `#[cfg(feature = "postgres")]`.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;
use tracing::info;

use super::backend::SqlValue;
use super::sqlite::SqliteDb;

/// Result of a test-connection attempt.
pub struct PgTestResult {
    pub version: String,
}

/// Create the target database when it does not exist yet, by connecting to
/// the maintenance `postgres` database on the same server. No user should
/// have to open psql just to run CREATE DATABASE (JP, PG migration).
/// Returns Ok(true) if the database was created, Ok(false) if nothing to do.
pub async fn ensure_database(url: &str) -> Result<bool, String> {
    // Split "…/dbname[?query]" — everything before the last '/' is the server part.
    let (server, rest) = url
        .rsplit_once('/')
        .ok_or_else(|| "invalid connection string (no database path)".to_string())?;
    let (db, query) = match rest.split_once('?') {
        Some((d, q)) => (d, Some(q)),
        None => (rest, None),
    };
    if db.is_empty() {
        return Err("invalid connection string (empty database name)".to_string());
    }
    // Guard the identifier: CREATE DATABASE cannot take a bind parameter, so
    // refuse anything that would need quoting/escaping.
    if !db
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "database name '{db}' contains unsupported characters"
        ));
    }
    let maint = match query {
        Some(q) => format!("{server}/postgres?{q}"),
        None => format!("{server}/postgres"),
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint)
        .await
        .map_err(|e| format!("maintenance connection failed: {e}"))?;
    let exists: bool = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
    )
    .bind(db)
    .fetch_one(&pool)
    .await
    .map_err(|e| format!("pg_database check failed: {e}"))?;
    if exists {
        pool.close().await;
        return Ok(false);
    }
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{db}\"")))
        .execute(&pool)
        .await
        .map_err(|e| format!("CREATE DATABASE failed: {e}"))?;
    pool.close().await;
    Ok(true)
}

/// Test a PostgreSQL connection: connect, run SELECT 1, fetch version.
pub async fn test_connection(url: &str) -> Result<PgTestResult, String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
        .map_err(|e| format!("connection failed: {e}"))?;

    // Verify the connection works
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("SELECT 1 failed: {e}"))?;

    let version: String = sqlx::query_scalar::<_, String>("SELECT version()")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("version query failed: {e}"))?;

    pool.close().await;
    Ok(PgTestResult { version })
}

/// Progress callback for the migration. Called after each table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationProgress {
    pub table: String,
    pub rows_copied: usize,
    pub tables_done: usize,
    pub tables_total: usize,
}

/// Final result of the migration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationResult {
    pub tables_migrated: usize,
    pub total_rows: usize,
    pub errors: Vec<String>,
    pub details: Vec<TableMigrationDetail>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TableMigrationDetail {
    pub table: String,
    pub rows: usize,
    pub skipped: bool,
}

/// Table migration order. Respects foreign key constraints:
/// parents before children.
const MIGRATION_TABLES: &[&str] = &[
    "settings",
    "profiles",
    "artists",
    "albums",
    "tracks",
    "track_credits",
    "track_metadata",
    "album_metadata",
    "playlists",
    "playlist_tracks",
    "zones",
    // Unified queue (v0.9 rc.2). Current SQLite DBs carry the queue here;
    // migrate_to_unified_queue() drops the legacy play_queue / streaming_queue
    // tables after copying, so those two below are only for pre-unification DBs
    // and are harmlessly skipped ("no columns or does not exist") when absent.
    // Without queue_items here the queue would NOT migrate at all.
    "queue_items",
    "play_queue",
    "streaming_queue",
    "listen_history",
    "radio_stations",
    "radio_favorites",
    "tags",
    "item_tags",
    // Etiquettes posees sur un album de streaming (#3699). Sans cette ligne,
    // tout l'etiquetage du catalogue Qobuz/Tidal/Bandcamp serait perdu a la
    // bascule SQLite -> PostgreSQL — et c'est justement sur le streaming que
    // l'etiquetage est le seul moyen de ranger.
    "streaming_item_tags",
    // Titres de service bannis (#4806). Sans cette ligne, tous les titres
    // Qobuz/Tidal/Bandcamp bannis redeviendraient jouables par l'aleatoire
    // et la radio a la bascule SQLite -> PostgreSQL.
    "streaming_hidden_items",
    "favorites",
    // Favoris de facette (#2442). Sans cette ligne, les labels mis en favori
    // seraient perdus à la bascule SQLite → PostgreSQL.
    "favorite_facets",
    // Albums masqués (#1391). Sans cette ligne, les albums masqués
    // réapparaîtraient tous à la bascule SQLite → PostgreSQL.
    "hidden_items",
    // « Ces deux albums ne sont pas des doublons » (#1276). Sans cette ligne,
    // l'arbitrage serait perdu à la bascule SQLite → PostgreSQL, et la fusion
    // de doublons emporterait au scan suivant ce que l'utilisateur avait
    // explicitement protégé.
    "album_distinct_pairs",
    // Appareils ignorés (#1280). Sans cette ligne, tous les appareils que
    // l'utilisateur a fait taire réapparaîtraient à la bascule
    // SQLite → PostgreSQL.
    "ignored_devices",
    // Dossiers de collections (#4853). Sans ces deux lignes, tout le
    // rangement de l'utilisateur serait perdu a la bascule SQLite ->
    // PostgreSQL — les collections, elles, passent (reglage + table).
    "collection_folders",
    "collection_folder_items",
    "album_ratings",
    "smart_playlists",
    "smart_collections",
    "bookmarks",
    "alarms",
    "network_mounts",
    // Registre des serveurs multimedia (#2219, phase 1). Juste apres
    // `network_mounts`, dont il reprend la forme — et pour la meme raison :
    // sans cette ligne, la bascule SQLite -> PostgreSQL rendrait au registre
    // le defaut qu'on vient de lui retirer, une liste qui repart de zero.
    "media_servers",
    "upnp_library_sources",
    "upnp_library_members",
    "podcast_subscriptions",
    "offline_cache",
    "sync_links",
    "sync_link_snapshots",
    "track_source_links",
];

/// The complete PG schema DDL. Creates all tables that exist in SQLite.
///
/// Numeric columns (year, track_number, duration_ms, sample_rate, …) are
/// declared TEXT here ON PURPOSE: `insert_batch`/`bind_migration_value` bind
/// every copied SQLite value as a TEXT parameter (SQLite is dynamically typed,
/// so this is the only universally-safe binding), and PG has no implicit
/// text→integer cast for an INSERT — a numeric column type here would make the
/// data copy fail. The intended numeric types are restored AFTER the copy by
/// the idempotent heal migrations run_pg_migrations() applies at PG startup
/// (010 albums/tracks, 011 listen_history, 012 the rest). The forced restart
/// into PostgreSQL after a migrate (routes/system/database.rs) guarantees that
/// convergence runs before the first real query. Do NOT switch these columns to
/// numeric types without also making the copy bind them natively.
///
/// Every CREATE TABLE uses IF NOT EXISTS and every INSERT for seed data
/// uses ON CONFLICT DO NOTHING, making this fully idempotent.
///
/// `pub(crate)` pour le seul test `pg_schema_parity`, qui monte ce schema et
/// celui des scripts numerotes dans deux bases distinctes et refuse tout ecart
/// (#2111).
pub(crate) const PG_FULL_SCHEMA: &str = r#"
-- Core tables
CREATE TABLE IF NOT EXISTS artists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    sort_name TEXT,
    musicbrainz_id TEXT,
    discogs_id TEXT,
    bio TEXT,
    bio_source TEXT,
    bio_source_url TEXT,
    bio_license TEXT,
    bio_lang TEXT,
    bio_fetched_at TEXT,
    image_path TEXT,
    image_source TEXT
);

CREATE TABLE IF NOT EXISTS albums (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    artist_id TEXT,
    year TEXT,
    original_year TEXT,
    genre TEXT,
    genres TEXT,
    disc_count TEXT DEFAULT 1,
    track_count TEXT DEFAULT 0,
    cover_path TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    label TEXT,
    catalog_number TEXT,
    barcode TEXT,
    format TEXT,
    sample_rate TEXT,
    bit_depth TEXT,
    bio TEXT,
    bio_source TEXT,
    bio_source_url TEXT,
    bio_license TEXT,
    bio_lang TEXT,
    bio_fetched_at TEXT,
    musicbrainz_release_id TEXT,
    musicbrainz_release_group_id TEXT,
    release_date TEXT,
    original_date TEXT,
    -- The folder on disk holding this release. What identifies an album: see
    -- `scanner::album_folder`.
    folder_path TEXT,
    -- Drapeau « compilation » (#1957). TEXT ici comme tout le reste de ce
    -- schéma de copie (voir l'en-tête) ; la migration PG 028 le ramène à
    -- SMALLINT après la copie.
    is_compilation TEXT,
    -- Type de sortie MusicBrainz (#4767) : `album`, `ep`, `single`,
    -- `broadcast`, `other`. TEXT des DEUX côtés — c'est un mot, pas un
    -- booléen — donc rien à reconvertir après la copie. NUL = inconnu.
    release_type TEXT,
    -- Curseur de la passe des crédits MusicBrainz (#4767). NUL = jamais
    -- interrogé.
    credits_mb_at TEXT,
    -- Source de la pochette et fichier d'origine (#5034, SQLite 111 / PG 074).
    -- NUL = inconnue. TEXT des deux côtés.
    cover_source TEXT,
    cover_source_path TEXT,
    cover_source_stamp TEXT
);

CREATE TABLE IF NOT EXISTS tracks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    album_id TEXT,
    artist_id TEXT,
    album_artist TEXT,
    disc_number TEXT DEFAULT 1,
    disc_subtitle TEXT,
    track_number TEXT DEFAULT 0,
    duration_ms TEXT DEFAULT 0,
    file_path TEXT UNIQUE,
    format TEXT,
    sample_rate TEXT,
    bit_depth TEXT,
    channels TEXT DEFAULT 2,
    file_mtime TEXT,
    file_size TEXT,
    audio_hash TEXT,
    audio_fingerprint TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    isrc TEXT,
    genre TEXT,
    genres TEXT,
    composer TEXT,
    year TEXT,
    bpm TEXT,
    label TEXT,
    musicbrainz_recording_id TEXT,
    comments TEXT,
    waveform_json TEXT,
    acoustid_fingerprint TEXT,
    acoustid_confidence TEXT,
    trailing_silence_ms TEXT,
    synced_lyrics TEXT,
    cover_path TEXT,
    -- Pistes virtuelles CUE (#1763) : cf le commentaire de CORE_SCHEMA.
    cue_media_path TEXT,
    cue_start_ms BIGINT,
    cue_end_ms BIGINT
);

CREATE TABLE IF NOT EXISTS track_credits (
    id TEXT PRIMARY KEY,
    track_id TEXT NOT NULL,
    artist_id TEXT,
    artist_name TEXT NOT NULL,
    role TEXT DEFAULT 'performer',
    instrument TEXT,
    position TEXT DEFAULT 0,
    artist_mbid TEXT
);

CREATE TABLE IF NOT EXISTS track_metadata (
    track_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (track_id, key)
);

CREATE TABLE IF NOT EXISTS album_metadata (
    album_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (album_id, key)
);

CREATE TABLE IF NOT EXISTS metadata_reports (
    id BIGSERIAL PRIMARY KEY,
    entity TEXT NOT NULL,
    entity_id BIGINT,
    mbid TEXT,
    field TEXT,
    value TEXT,
    reason TEXT NOT NULL,
    comment TEXT,
    created_at TEXT NOT NULL,
    pushed_at TEXT
);

CREATE TABLE IF NOT EXISTS playlists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    profile_id TEXT NOT NULL DEFAULT '1'
);

-- #4889 : `track_id` nul pour un titre de service, porte par `source` /
-- `source_id` et ses colonnes d'affichage (migration PG 072, SQLite 109).
-- Tout en TEXT comme le reste de ce schema : la 072 convertit `duration_ms`.
CREATE TABLE IF NOT EXISTS playlist_tracks (
    id TEXT PRIMARY KEY,
    playlist_id TEXT NOT NULL,
    track_id TEXT,
    position TEXT NOT NULL DEFAULT 0,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    album_source_id TEXT,
    duration_ms TEXT,
    cover_url TEXT
);

CREATE SEQUENCE IF NOT EXISTS zones_id_seq;
CREATE TABLE IF NOT EXISTS zones (
    id TEXT PRIMARY KEY DEFAULT nextval('zones_id_seq')::text,
    name TEXT NOT NULL,
    output_type TEXT,
    output_device_id TEXT,
    volume TEXT DEFAULT 50,
    muted TEXT DEFAULT 0,
    online TEXT DEFAULT 1,
    gapless_enabled TEXT DEFAULT 1,
    group_id TEXT,
    sync_delay_ms TEXT NOT NULL DEFAULT 0,
    last_position_ms TEXT NOT NULL DEFAULT 0,
    last_track_id TEXT,
    last_track_source TEXT,
    last_track_source_id TEXT,
    max_sample_rate TEXT,
    fixed_volume TEXT DEFAULT 0,
    dsp_preset_id TEXT,
    dsp_enabled TEXT DEFAULT 0,
    autoplay_enabled TEXT DEFAULT 0,
    is_hidden TEXT DEFAULT 0,
    last_play_state TEXT DEFAULT 'stopped',
    dsd_mode TEXT DEFAULT 'auto',
    dlna_native_flac TEXT DEFAULT 0,
    host TEXT,
    alac_passthrough TEXT DEFAULT 0,
    aac_passthrough TEXT DEFAULT 0,
    dlna_lpcm TEXT DEFAULT 0,
    dlna_cap_16bit TEXT DEFAULT 0,
    lyrics_offset_ms TEXT DEFAULT 0,
    last_seen_at TEXT,
    output_endpoint_id TEXT,
    -- Motif et date du masquage (#5077, SQLite 112 / PG 075). NUL = inconnu.
    -- TEXT des deux côtés.
    motif_masquage TEXT,
    masquee_le TEXT
);

CREATE TABLE IF NOT EXISTS play_queue (
    id TEXT PRIMARY KEY,
    zone_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    position TEXT NOT NULL DEFAULT 0,
    is_current TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS streaming_queue (
    id TEXT PRIMARY KEY,
    zone_id TEXT NOT NULL,
    position TEXT NOT NULL,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    duration_ms TEXT DEFAULT 0
);

-- Unified queue (v0.9 rc.2): replaces the play_queue / streaming_queue split.
CREATE TABLE IF NOT EXISTS queue_items (
    id TEXT PRIMARY KEY,
    zone_id TEXT NOT NULL,
    position TEXT NOT NULL DEFAULT 0,
    is_current TEXT DEFAULT 0,
    track_id TEXT,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    duration_ms TEXT DEFAULT 0,
    track_number TEXT,
    disc_number TEXT
);

-- One-time copy of the split tables into queue_items. IDs are prefixed to
-- avoid collisions between the two source tables, so the copy only makes
-- sense while `queue_items.id` is still the TEXT column declared above.
--
-- 🔴 Deux gardes, pas une. « queue_items est vide » ne suffisait pas : une
-- bascule REJOUEE sur une base deja basculee trouve `queue_items` vide (la
-- file SQLite l'etait) mais son `id` deja converti en BIGINT par la
-- migration 012 (`run_pg_migrations`, en fin de bascule). L'INSERT de
-- `'lq_' || id` y echouait — « column "id" is of type bigint but expression
-- is of type text » — et avec lui TOUTE la creation du schema : la bascule
-- ne se relancait plus (#5199, releve par #5139). Une base native (001, BIGSERIAL)
-- est dans le meme cas. Le type se lit dans le catalogue ; tant qu'il n'est
-- pas TEXT, l'INSERT n'est meme pas planifie (PL/pgSQL planifie a la
-- premiere execution).
DO $$
BEGIN
    IF (SELECT data_type FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'queue_items' AND column_name = 'id')
           IN ('text', 'character varying')
       AND NOT EXISTS (SELECT 1 FROM queue_items LIMIT 1) THEN
        INSERT INTO queue_items (id, zone_id, position, is_current, track_id, source, duration_ms)
            SELECT 'lq_' || id, zone_id, position, is_current, track_id, 'local', '0' FROM play_queue;
        INSERT INTO queue_items (id, zone_id, position, is_current, source, source_id, title, artist, album, cover_url, duration_ms)
            SELECT 'sq_' || id, zone_id, position, '0', source, source_id, title, artist, album, cover_url, duration_ms FROM streaming_queue;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS listen_history (
    id TEXT PRIMARY KEY,
    track_id TEXT,
    title TEXT NOT NULL,
    artist_name TEXT,
    album_title TEXT,
    source TEXT DEFAULT 'local',
    duration_ms TEXT DEFAULT 0,
    listened_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    zone_id TEXT,
    cover_url TEXT,
    source_id TEXT,
    album_id TEXT,
    profile_id TEXT,
    context_type TEXT,
    context_id TEXT,
    context_position TEXT,
    context_source TEXT,
    context_title TEXT,
    context_cover TEXT
);

CREATE TABLE IF NOT EXISTS radio_stations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    homepage TEXT,
    logo_url TEXT,
    country TEXT,
    language TEXT,
    genre TEXT,
    codec TEXT,
    bitrate TEXT,
    is_favorite TEXT DEFAULT 0,
    last_played TEXT,
    play_count TEXT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS radio_favorites (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT DEFAULT '',
    station_name TEXT DEFAULT '',
    cover_url TEXT,
    stream_url TEXT,
    saved_at TEXT,
    UNIQUE(title, artist)
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS profiles (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    avatar_path TEXT,
    password_hash TEXT,
    is_admin TEXT DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    email TEXT,
    password_hash_v2 TEXT
);

-- Favoris de VALEUR de facette (label…), #2442. Pas de colonne `id` : la clé
-- naturelle est la clé primaire. `profile_id` en TEXT comme partout ici (la
-- copie lie tout en texte) ; la migration 036 la ramène en BIGINT après coup.
CREATE TABLE IF NOT EXISTS favorite_facets (
    profile_id TEXT NOT NULL DEFAULT '1',
    facet TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, facet, value)
);

CREATE TABLE IF NOT EXISTS favorites (
    id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    item_name TEXT,
    item_artist TEXT,
    item_path TEXT,
    position TEXT,
    UNIQUE(profile_id, item_type, item_id)
);
-- Instantané d'identité des favoris (SQLite v66 / PG 017) : nécessaire ici
-- aussi pour qu'une base PG créée par cette migration de données accepte la
-- copie des colonnes présentes côté SQLite.
ALTER TABLE favorites ADD COLUMN IF NOT EXISTS item_name TEXT;
ALTER TABLE favorites ADD COLUMN IF NOT EXISTS item_artist TEXT;
ALTER TABLE favorites ADD COLUMN IF NOT EXISTS item_path TEXT;
-- Rang manuel (#2001 piste 2, SQLite 100 / PG 057). TEXT comme le reste de ce
-- schéma miroir : la copie SQLite→PostgreSQL lie chaque valeur en texte et
-- PostgreSQL n'a pas de cast implicite texte→entier à l'INSERT. `as_i64`
-- reconvertit à la relecture, et le rang est comparé en Rust — donc jamais par
-- un ORDER BY qui mettrait « 10 » avant « 2 » ici et pas sur SQLite.
ALTER TABLE favorites ADD COLUMN IF NOT EXISTS position TEXT;

-- Albums masqués (#1391). Tout en TEXT comme le reste de ce schéma (la copie
-- lie chaque valeur en texte) ; la migration 041 ramène `profile_id` et
-- `item_id` en BIGINT après coup, comme 038 pour `favorite_facets`.
CREATE TABLE IF NOT EXISTS hidden_items (
    profile_id TEXT NOT NULL DEFAULT '1',
    item_type TEXT NOT NULL,
    item_id TEXT NOT NULL,
    item_name TEXT,
    item_artist TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, item_type, item_id)
);

-- « Ces deux albums ne sont pas des doublons » (#1276). Tout en TEXT comme le
-- reste de ce schéma (la copie lie chaque valeur en texte) ; la migration 042
-- ramène `profile_id`, `album_a_id` et `album_b_id` en BIGINT après coup,
-- comme 041 pour `hidden_items`.
CREATE TABLE IF NOT EXISTS album_distinct_pairs (
    profile_id TEXT NOT NULL DEFAULT '1',
    album_a_id TEXT NOT NULL,
    album_b_id TEXT NOT NULL,
    a_name TEXT,
    a_artist TEXT,
    b_name TEXT,
    b_artist TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, album_a_id, album_b_id)
);

-- Appareils ignorés (#1280). Tout en TEXT — comme la migration 044, qui
-- déclare exactement les mêmes colonnes : rien à rattraper après la copie,
-- contrairement à `hidden_items` et `favorite_facets`.
CREATE TABLE IF NOT EXISTS ignored_devices (
    device_id TEXT PRIMARY KEY,
    mac TEXT NOT NULL DEFAULT '',
    host TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '',
    device_type TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- Dossiers de collections (#4853). Tout en TEXT, comme le reste de ce
-- schéma : la copie lie chaque valeur SQLite en texte. La migration 071
-- convertit ensuite les colonnes entières en BIGINT (conversion gardée sur le
-- type courant, patron 038).
CREATE TABLE IF NOT EXISTS collection_folders (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    parent_id TEXT,
    position TEXT NOT NULL DEFAULT '0',
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE TABLE IF NOT EXISTS collection_folder_items (
    kind TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    folder_id TEXT,
    position TEXT NOT NULL DEFAULT '0',
    PRIMARY KEY (kind, collection_id)
);

CREATE SEQUENCE IF NOT EXISTS streaming_favorites_id_seq;
CREATE TABLE IF NOT EXISTS streaming_favorites (
    id TEXT PRIMARY KEY DEFAULT nextval('streaming_favorites_id_seq')::text,
    profile_id TEXT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    service TEXT NOT NULL,
    service_id TEXT NOT NULL,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    position TEXT,
    UNIQUE(profile_id, item_type, service, service_id)
);
-- Rattrapage de la bascule : une base créée par une version antérieure de ce
-- schéma a la table SANS `position`, et le `CREATE TABLE IF NOT EXISTS`
-- ci-dessus ne la corrige pas. Même raison que les trois colonnes d'identité de
-- `favorites` juste au-dessus (#2111).
ALTER TABLE streaming_favorites ADD COLUMN IF NOT EXISTS position TEXT;
-- Date de première vue LOCALE (web #1060) — même rattrapage, même raison.
ALTER TABLE streaming_favorites ADD COLUMN IF NOT EXISTS first_seen_at TEXT;

CREATE TABLE IF NOT EXISTS tags (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT DEFAULT '#808080'
);

CREATE TABLE IF NOT EXISTS item_tags (
    id TEXT PRIMARY KEY,
    tag_id TEXT NOT NULL,
    item_type TEXT NOT NULL,
    item_id TEXT NOT NULL,
    UNIQUE(tag_id, item_type, item_id)
);

-- Etiquettes posees sur un objet de STREAMING (#3699). Une base creee par la
-- bascule SQLite -> PostgreSQL enregistre `schema_version = 99` et ne rejoue
-- JAMAIS les scripts numerotes : sans cette declaration ici, la migration 052
-- ne l'atteindrait pas — ni maintenant, ni jamais.
--
-- `tag_id` est BIGINT et non TEXT, contrairement au reste de ce schema : la
-- copie de donnees lie NATIVEMENT les entiers (voir `insert_batch`), et il
-- faut que `streaming_item_tags.tag_id` reste comparable a `tags.id`, que la
-- migration 012 remet en BIGINT sur ces bases-la.
CREATE TABLE IF NOT EXISTS streaming_item_tags (
    tag_id BIGINT NOT NULL,
    item_type TEXT NOT NULL,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (tag_id, item_type, source, source_id)
);

-- Titres de SERVICE bannis (#4806) — jumelle de `hidden_items` pour la paire
-- `source` + `source_id`. `profile_id` en BIGINT et non TEXT : la copie lie
-- NATIVEMENT les entiers, et les filtres le comparent a un entier.
CREATE TABLE IF NOT EXISTS streaming_hidden_items (
    profile_id BIGINT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    title TEXT,
    artist TEXT,
    album TEXT,
    album_source_id TEXT,
    cover_url TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, item_type, source, source_id)
);

-- Registre DURABLE des serveurs multimedia (#2219, phase 1). Une base creee
-- par la bascule SQLite -> PostgreSQL enregistre `schema_version = 99` et ne
-- rejoue JAMAIS les scripts numerotes : sans cette declaration ici, la
-- migration 058 ne l'atteindrait pas — ni maintenant, ni jamais.
--
-- Meme forme que `network_mounts` : `active` dit l'INTENTION, `last_state` et
-- `absence_reason` disent le CONSTAT. `udn` est la clef primaire — l'identite
-- stable d'un appareil UPnP, que le changement de port ne touche pas.
CREATE TABLE IF NOT EXISTS media_servers (
    udn TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    manufacturer TEXT,
    model TEXT,
    device_type TEXT NOT NULL DEFAULT 'upnp_media_server',
    location TEXT NOT NULL DEFAULT '',
    content_directory_url TEXT,
    host TEXT,
    port INTEGER,
    max_age_secs INTEGER,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    last_state TEXT,
    absence_reason TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS album_ratings (
    id TEXT PRIMARY KEY,
    album_id TEXT NOT NULL,
    profile_id TEXT NOT NULL DEFAULT 1,
    rating TEXT NOT NULL,
    note TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(album_id, profile_id)
);

CREATE TABLE IF NOT EXISTS file_first_seen (
    file_path TEXT PRIMARY KEY,
    first_seen_at DOUBLE PRECISION NOT NULL
);

CREATE TABLE IF NOT EXISTS streaming_auth (
    service TEXT PRIMARY KEY,
    token_data TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS smart_playlists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    sort_by TEXT DEFAULT 'title',
    sort_order TEXT DEFAULT 'asc',
    max_tracks TEXT,
    match_mode TEXT NOT NULL DEFAULT 'all',
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS smart_collections (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    match_mode TEXT NOT NULL DEFAULT 'all',
    sort_by TEXT,
    sort_order TEXT NOT NULL DEFAULT 'asc',
    max_limit TEXT,
    created_at TEXT,
    updated_at TEXT,
    description TEXT,
    icon TEXT,
    color TEXT
);

CREATE TABLE IF NOT EXISTS bookmarks (
    id TEXT PRIMARY KEY,
    track_id TEXT,
    position_ms TEXT NOT NULL DEFAULT 0,
    label TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS alarms (
    id TEXT PRIMARY KEY,
    zone_id TEXT,
    time TEXT NOT NULL,
    enabled TEXT DEFAULT 1,
    days TEXT DEFAULT '1,2,3,4,5,6,7',
    source_type TEXT DEFAULT 'playlist',
    source_id TEXT,
    volume TEXT DEFAULT 0.3,
    fade_in_seconds TEXT DEFAULT 30,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    name TEXT DEFAULT 'Alarm',
    one_shot TEXT DEFAULT 0,
    skip_holidays TEXT DEFAULT 0,
    source_name TEXT,
    fade_duration_s TEXT DEFAULT 60,
    last_fired_at TEXT,
    days_of_week TEXT DEFAULT '1111111',
    multi_zone_ids TEXT
);

CREATE TABLE IF NOT EXISTS network_mounts (
    id TEXT PRIMARY KEY,
    mount_type TEXT NOT NULL DEFAULT 'smb',
    server TEXT NOT NULL,
    share TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    username TEXT,
    password TEXT,
    active TEXT DEFAULT 1,
    -- `active` = l'intention, les trois suivantes = le constat du dernier
    -- montage. Voir migrations/postgres/027 et tune-server/src/smb.rs.
    smb_version TEXT,
    mount_state TEXT,
    last_mount_error TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS podcast_subscriptions (
    id TEXT PRIMARY KEY,
    feed_url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    author TEXT,
    image_url TEXT,
    description TEXT,
    source_id TEXT,
    last_checked TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE TABLE IF NOT EXISTS offline_cache (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    track_title TEXT,
    artist_name TEXT,
    album_title TEXT,
    file_path TEXT,
    file_size TEXT,
    duration_ms TEXT,
    quality TEXT,
    downloaded_at TEXT,
    expires_at TEXT,
    status TEXT DEFAULT 'pending',
    error TEXT,
    UNIQUE(source, source_id)
);

CREATE TABLE IF NOT EXISTS sync_links (
    id TEXT PRIMARY KEY,
    local_playlist_id TEXT NOT NULL,
    service TEXT NOT NULL,
    remote_playlist_id TEXT NOT NULL,
    direction TEXT NOT NULL DEFAULT 'bidirectional',
    last_synced TEXT,
    created_at TEXT
);

CREATE TABLE IF NOT EXISTS sync_link_snapshots (
    id TEXT PRIMARY KEY,
    playlist_link_id TEXT NOT NULL,
    side TEXT NOT NULL,
    tracks_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS track_source_links (
    id TEXT PRIMARY KEY,
    track_id TEXT NOT NULL,
    service TEXT NOT NULL,
    service_track_id TEXT NOT NULL,
    confidence TEXT NOT NULL DEFAULT 0.0,
    match_method TEXT,
    linked_at TEXT DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(track_id, service)
);

-- LRCLIB lyrics cache (SQLite migration v43 / PG script 008). Present here
-- too because a database created by THIS sqlite→pg migration records
-- schema_version 99 and therefore never replays the numbered PG scripts:
-- without this block such a DB has no lyrics_cache at all (drift).
CREATE TABLE IF NOT EXISTS lyrics_cache (
    track_id BIGINT PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    synced_lyrics TEXT,
    plain_lyrics TEXT,
    source TEXT NOT NULL DEFAULT 'lrclib',
    fetched_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- Registre des executions automatisees (#2080 ; migration PG 039). Present ici
-- pour la meme raison que `lyrics_cache` juste au-dessus : une base creee par
-- cette bascule sqlite->pg enregistre schema_version 99 et ne rejoue donc
-- jamais les scripts PG numerotes. Sans ce bloc, une telle base n'aurait pas
-- de `task_runs` du tout, et la route d'observabilite rendrait une erreur SQL.
--
-- Cette table n'est PAS dans `MIGRATION_TABLES` : ses `boot_id` designent des
-- incarnations d'un processus qui tournait sur l'AUTRE moteur. L'historique
-- d'observabilite recommence avec le nouveau moteur, deliberement.
CREATE TABLE IF NOT EXISTS task_runs (
    boot_id TEXT NOT NULL,
    task TEXT NOT NULL,
    seq BIGINT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    duration_ms BIGINT,
    outcome TEXT NOT NULL,
    items BIGINT,
    detail TEXT,
    PRIMARY KEY (boot_id, task, seq)
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_task_runs_task_started ON task_runs(task, started_at);
CREATE INDEX IF NOT EXISTS idx_task_runs_outcome ON task_runs(outcome);
CREATE INDEX IF NOT EXISTS idx_tracks_file_path ON tracks(file_path);
CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_audio_hash ON tracks(audio_hash);
CREATE INDEX IF NOT EXISTS idx_tracks_source_source_id ON tracks(source, source_id);
CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_track_id ON track_credits(track_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist_id ON track_credits(artist_id);
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist_id ON playlist_tracks(playlist_id);
CREATE INDEX IF NOT EXISTS idx_play_queue_zone_id ON play_queue(zone_id);
CREATE INDEX IF NOT EXISTS idx_listen_history_listened_at ON listen_history(listened_at);
CREATE INDEX IF NOT EXISTS idx_listen_history_track_id ON listen_history(track_id);
CREATE INDEX IF NOT EXISTS idx_radio_stations_favorite ON radio_stations(is_favorite);
CREATE INDEX IF NOT EXISTS idx_bookmarks_track_id ON bookmarks(track_id);
CREATE INDEX IF NOT EXISTS idx_favorites_profile ON favorites(profile_id, item_type);
CREATE INDEX IF NOT EXISTS idx_item_tags_item ON item_tags(item_type, item_id);
CREATE INDEX IF NOT EXISTS idx_streaming_item_tags_item ON streaming_item_tags(item_type, source, source_id);
CREATE INDEX IF NOT EXISTS idx_streaming_hidden_items_item ON streaming_hidden_items(item_type, source, source_id);
CREATE TABLE IF NOT EXISTS upnp_library_sources (
    source_key TEXT PRIMARY KEY,
    udn TEXT NOT NULL,
    container TEXT NOT NULL,
    state_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS upnp_library_members (
    source_key TEXT NOT NULL REFERENCES upnp_library_sources(source_key) ON DELETE CASCADE,
    track_id BIGINT NOT NULL,
    generation TEXT NOT NULL,
    PRIMARY KEY (source_key, track_id)
);

CREATE INDEX IF NOT EXISTS idx_upnp_library_members_track ON upnp_library_members(track_id);
CREATE INDEX IF NOT EXISTS idx_media_servers_last_seen ON media_servers(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_album_ratings_album ON album_ratings(album_id);
CREATE INDEX IF NOT EXISTS idx_track_metadata_key ON track_metadata(key);
CREATE INDEX IF NOT EXISTS idx_album_metadata_key ON album_metadata(key);
CREATE INDEX IF NOT EXISTS idx_track_source_links_track ON track_source_links(track_id);
CREATE INDEX IF NOT EXISTS idx_track_source_links_service ON track_source_links(service);
CREATE INDEX IF NOT EXISTS idx_offline_cache_source ON offline_cache(source, source_id);
CREATE INDEX IF NOT EXISTS idx_offline_cache_status ON offline_cache(status);
CREATE INDEX IF NOT EXISTS idx_sync_snapshots_link ON sync_link_snapshots(playlist_link_id, side);

-- Schema version tracking. `version` MUST be INTEGER: the migration runner
-- (migrations.rs run_pg_migrations) and the numbered SQL scripts read/insert
-- integer versions, and a TEXT column made `COALESCE(MAX(version), 0)` abort
-- at startup on data-migrated databases (JF, v0.9.13). The runner also heals
-- existing TEXT columns in place at startup.
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TIMESTAMPTZ DEFAULT now(),
    name TEXT NOT NULL
);
-- Re-migration onto a database created before the INTEGER fix: convert the
-- legacy TEXT column in place BEFORE the INSERT below touches it. Values are
-- digit strings, the cast is safe; no-op (trivial rewrite) when already INTEGER.
ALTER TABLE schema_version ALTER COLUMN version TYPE INTEGER USING version::integer;
INSERT INTO schema_version (version, name) VALUES (99, 'sqlite_migration')
    ON CONFLICT (version) DO NOTHING;

-- ─────────────────────────────────────────────────────────────────────
-- Idempotent column back-fills for ALREADY-CREATED PG databases.
--
-- A PG database created by an EARLIER version of this migration is missing
-- columns that later SQLite migrations added (bio provenance, zone playback/
-- DLNA flags, listen_history profile scoping, smart_playlists match_mode).
-- Without these, the data copy fails with `column "X" does not exist` and the
-- whole table is silently skipped (tester JP, forum). `ADD COLUMN IF NOT EXISTS`
-- brings such a DB up to date without a manual drop; it is a no-op on a fresh DB
-- (the CREATE TABLEs above already carry these columns). Types/defaults mirror
-- the CREATE TABLEs (booleans stay TEXT 0/1 to match the copied SQLite data).
-- ─────────────────────────────────────────────────────────────────────

-- artists / albums: bio provenance (SQLite migration v54)
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_source TEXT;
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_source_url TEXT;
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_license TEXT;
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_lang TEXT;
ALTER TABLE artists ADD COLUMN IF NOT EXISTS bio_fetched_at TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_source TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_source_url TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_license TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_lang TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS bio_fetched_at TEXT;

-- albums: drapeau « compilation » (SQLite migration v79, #1957). TEXT 0/1 comme
-- les autres booléens copiés ; la migration PG 028 le ramène à SMALLINT après.
ALTER TABLE albums ADD COLUMN IF NOT EXISTS is_compilation TEXT DEFAULT 0;

-- albums: type de sortie MusicBrainz (SQLite migration v106, #4767). Sans
-- défaut : NUL veut dire « inconnu », et c'est l'état normal — la couverture
-- MBID mesurée est de 0,9 % sur le .18. `select_album` NOMME cette colonne.
ALTER TABLE albums ADD COLUMN IF NOT EXISTS release_type TEXT;

-- Crédits MusicBrainz par disque (SQLite migration v107, #4767) : l'artiste
-- crédité par son MBID, et le curseur de reprise de la passe.
ALTER TABLE track_credits ADD COLUMN IF NOT EXISTS artist_mbid TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS credits_mb_at TEXT;

-- Source de la pochette d'album (SQLite migration v111, #5034). Sans défaut :
-- NUL = inconnue, l'état de toute ligne d'avant la migration.
ALTER TABLE albums ADD COLUMN IF NOT EXISTS cover_source TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS cover_source_path TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS cover_source_stamp TEXT;

-- alarms: owning profile (SQLite migration v64)
ALTER TABLE alarms ADD COLUMN IF NOT EXISTS profile_id BIGINT;

-- zones: playback state + DLNA/DSD flags (SQLite migrations v36/v38/v39/v40/v50
-- and the post-migration safety pass: alac_passthrough / dlna_lpcm /
-- dlna_cap_16bit / host)
ALTER TABLE zones ADD COLUMN IF NOT EXISTS autoplay_enabled TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS is_hidden TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS last_play_state TEXT DEFAULT 'stopped';
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dsd_mode TEXT DEFAULT 'auto';
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_native_flac TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS host TEXT;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS mac TEXT;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS alac_passthrough TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS aac_passthrough TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_lpcm TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_cap_16bit TEXT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS last_seen_at TEXT;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS output_endpoint_id TEXT;
-- Motif et date du masquage d'une zone (SQLite migration v112, #5077). Sans
-- défaut : NUL = inconnu, l'état de tout masquage d'avant la migration.
ALTER TABLE zones ADD COLUMN IF NOT EXISTS motif_masquage TEXT;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS masquee_le TEXT;

-- listen_history: streaming source id + album id + profile scoping (v32/v37/v45)
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS source_id TEXT;
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS album_id TEXT;
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS profile_id TEXT;
-- listen_history: ce que l'auditeur a demande (SQLite migration v84, #2441)
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_type TEXT;
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_id TEXT;
-- listen_history: ou l'auditeur en etait dans cet objet (SQLite migration v94,
-- #2441). TEXT comme album_id / profile_id ici : ce schema porte tout en TEXT.
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_position TEXT;
-- listen_history: l'espace de noms de context_id, et le nom d'une playlist de
-- service que cette base ne sait pas retrouver (SQLite migration v105).
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_source TEXT;
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_title TEXT;
ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS context_cover TEXT;

-- smart_playlists: match_mode (SQLite migration v48)
ALTER TABLE smart_playlists ADD COLUMN IF NOT EXISTS match_mode TEXT NOT NULL DEFAULT 'all';

-- podcast_subscriptions: streaming source id (SQLite migration v59)
ALTER TABLE podcast_subscriptions ADD COLUMN IF NOT EXISTS source_id TEXT;

-- queue_items: per-album numbering for streaming tracks (SQLite migration v64)
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS track_number TEXT;
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS disc_number TEXT;

-- tracks: colonnes du chantier CUE (SQLite migration v76, #1763). Elles sont
-- ici pour la meme raison que les autres : la copie qui suit lit la table
-- SQLite colonne par colonne, et la source les possede. Sans ce rattrapage,
-- une base PG creee par une version anterieure de ce schema ferait echouer
-- l'INSERT de `tracks` en entier — la bibliotheque arriverait vide. La
-- migration 031 repare le meme manque pour les bases montees par les scripts
-- numerotes, qui ne passent jamais par ici (#2111).
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cue_media_path TEXT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cue_start_ms BIGINT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cue_end_ms BIGINT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS audio_fingerprint TEXT;
"#;

/// Post-copy normalisation: `tracks.file_mtime` is canonically DOUBLE
/// PRECISION (001 creates it DOUBLE on fresh installs; an mtime is numeric by
/// nature, and `file_first_seen.first_seen_at` is already DOUBLE).
/// `PG_FULL_SCHEMA` above deliberately creates it TEXT — the data copy binds
/// every parameter as text, so converting BEFORE the copy would abort the
/// whole `tracks` INSERT ("column is of type double precision but expression
/// is of type text") and the library would arrive empty. Migration 013,
/// replayed by `run_pg_migrations` right after the copy, normally performs
/// the TEXT → DOUBLE conversion; this block repeats it so a failure in an
/// earlier numbered migration cannot strand the drifted TEXT column (that
/// TEXT/DOUBLE drift between install vintages is what broke the added_at
/// album sort — see album_repo.rs list_sorted). Idempotent: strict no-op once
/// the column is DOUBLE. '' (SQLite dynamic-typing garbage carried over by
/// the text copy) becomes NULL, like any non-numeric value (regex guard
/// mirrors migration 013). No Rust write path can produce '' — the model
/// field is `Option<f64>` and binds natively — so once converted the column
/// stays clean.
pub(crate) const PG_NORMALIZE_FILE_MTIME: &str = r#"
DO $$
DECLARE
    cur_type TEXT;
BEGIN
    SELECT data_type INTO cur_type
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'tracks' AND column_name = 'file_mtime';
    IF cur_type IN ('text', 'character varying') THEN
        UPDATE tracks SET file_mtime = NULL WHERE file_mtime = '';
        ALTER TABLE tracks ALTER COLUMN file_mtime TYPE DOUBLE PRECISION
            USING (CASE WHEN file_mtime ~ '^-?[0-9]+(\.[0-9]+)?$'
                        THEN file_mtime::double precision END);
        RAISE NOTICE 'pg_migrate: tracks.file_mtime % -> double precision', cur_type;
    END IF;
END $$;
"#;

/// Run the full SQLite → PostgreSQL migration.
///
/// 1. Connects to PG at `pg_url`
/// 2. Creates all tables (idempotent)
/// 3. Copies data table by table from `sqlite_db`
/// 4. Returns a summary
pub async fn migrate_sqlite_to_pg(
    sqlite_db: &SqliteDb,
    pg_url: &str,
) -> Result<MigrationResult, String> {
    let start = std::time::Instant::now();

    // Connect to PG
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(15))
        .connect(pg_url)
        .await
        .map_err(|e| format!("pg connect: {e}"))?;

    info!("pg_migrate: connected");

    // Create schema
    sqlx::raw_sql(PG_FULL_SCHEMA)
        .execute(&pool)
        .await
        .map_err(|e| format!("pg schema creation failed: {e}"))?;

    info!("pg_migrate: schema created");

    let mut result = MigrationResult {
        tables_migrated: 0,
        total_rows: 0,
        errors: Vec::new(),
        details: Vec::new(),
    };

    let tables_total = MIGRATION_TABLES.len();

    for (idx, table_name) in MIGRATION_TABLES.iter().enumerate() {
        match migrate_table(sqlite_db, &pool, table_name).await {
            Ok(rows) => {
                info!(
                    table = table_name,
                    rows,
                    progress = format!("{}/{}", idx + 1, tables_total),
                    "pg_migrate_table_done"
                );
                result.tables_migrated += 1;
                result.total_rows += rows;
                result.details.push(TableMigrationDetail {
                    table: table_name.to_string(),
                    rows,
                    skipped: false,
                });
            }
            Err(e) => {
                // Table might not exist in SQLite (e.g. streaming_queue
                // is lazily created). Log and continue.
                let msg = format!("{table_name}: {e}");
                info!(table = table_name, error = %e, "pg_migrate_table_skipped");
                result.errors.push(msg);
                result.details.push(TableMigrationDetail {
                    table: table_name.to_string(),
                    rows: 0,
                    skipped: true,
                });
            }
        }
    }

    // Bring the freshly-migrated database up to the latest schema right now —
    // most importantly migration 012, which converts the TEXT `id` columns this
    // schema creates back to auto-incrementing BIGINT with a sequence (a fresh
    // install gets BIGSERIAL from 001). Without it the migrated DB inherits
    // SQLite's dynamic typing and rejects every NEW insert that omits `id`
    // ("null value in column \"id\" ... violates not-null constraint" — the
    // scan sees the files but writes 0, JF). Startup runs the migrations too,
    // but the migrate route only restarts when persisting the DATABASE_URL
    // succeeds, so doing it here guarantees a usable database regardless.
    // Idempotent: the startup pass then finds nothing to do.
    if let Err(e) = crate::db::migrations::run_pg_migrations(&pool).await {
        tracing::warn!(error = %e, "pg_migrate_post_migration_upgrade_failed");
        result
            .errors
            .push(format!("post-migration schema upgrade failed: {e}"));
    }

    // Belt and braces: even if the upgrade above failed before migration 013
    // could run, tracks.file_mtime must leave here with its canonical DOUBLE
    // PRECISION type (see PG_NORMALIZE_FILE_MTIME). No-op when 013 already
    // converted it.
    if let Err(e) = sqlx::raw_sql(PG_NORMALIZE_FILE_MTIME).execute(&pool).await {
        tracing::warn!(error = %e, "pg_migrate_file_mtime_normalize_failed");
        result
            .errors
            .push(format!("file_mtime normalisation failed: {e}"));
    }

    let elapsed = start.elapsed();
    info!(
        tables = result.tables_migrated,
        rows = result.total_rows,
        errors = result.errors.len(),
        duration_ms = elapsed.as_millis() as u64,
        "pg_migrate_complete"
    );

    pool.close().await;
    Ok(result)
}

/// Migrate a single table from SQLite to PG.
///
/// Reads all rows from SQLite, then inserts them in batches of 1000
/// using ON CONFLICT DO NOTHING for idempotence.
async fn migrate_table(sqlite_db: &SqliteDb, pool: &PgPool, table: &str) -> Result<usize, String> {
    // First, discover the column names from SQLite
    let columns = get_sqlite_columns(sqlite_db, table)?;
    if columns.is_empty() {
        return Err(format!("table {table} has no columns or does not exist"));
    }

    // Read all rows from SQLite
    let col_list = columns.join(", ");
    let sql = format!("SELECT {col_list} FROM {table}");

    let rows: Vec<Vec<SqlValue>> = {
        let conn = sqlite_db.read_connection();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare SELECT from {table}: {e}"))?;
        let col_count = stmt.column_count();
        let mut result_rows = Vec::new();
        let mut query_rows = stmt.query([]).map_err(|e| format!("query {table}: {e}"))?;
        while let Some(row) = query_rows.next().map_err(|e| format!("row {table}: {e}"))? {
            let mut vals = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v = row
                    .get_ref(i)
                    .map(|vr| match vr {
                        rusqlite::types::ValueRef::Null => SqlValue::Null,
                        rusqlite::types::ValueRef::Integer(n) => SqlValue::Int(n),
                        rusqlite::types::ValueRef::Real(f) => SqlValue::Real(f),
                        rusqlite::types::ValueRef::Text(b) => {
                            SqlValue::Text(String::from_utf8_lossy(b).into_owned())
                        }
                        rusqlite::types::ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
                    })
                    .map_err(|e| format!("col {i} in {table}: {e}"))?;
                vals.push(v);
            }
            result_rows.push(vals);
        }
        result_rows
    };

    if rows.is_empty() {
        return Ok(0);
    }

    let total = rows.len();
    let batch_size = 1000;
    let mut copied = 0;

    let conflict_clause = conflict_clause(table);
    let casts = column_casts(pool, table, &columns).await?;

    for chunk in rows.chunks(batch_size) {
        insert_batch(pool, table, &columns, &casts, chunk, conflict_clause).await?;
        copied += chunk.len();
        if total > 5000 && copied % 5000 == 0 {
            info!(table, copied, total, "pg_migrate_batch_progress");
        }
    }

    Ok(total)
}

/// La clause `ON CONFLICT` de la copie d'une table.
///
/// Elle doit viser la CLEF de la table — sa clef primaire ou un index
/// unique — telle que `PG_FULL_SCHEMA` la declare. PostgreSQL refuse une
/// cible qui n'en est pas une, et le refus fait tomber TOUT le lot : la table
/// part alors en `pg_migrate_table_skipped`, ses lignes restent en SQLite.
/// Le test `pg_bascule_chaque_clause_vise_une_clef` confronte chaque table de
/// `MIGRATION_TABLES` au catalogue d'un vrai PostgreSQL.
fn conflict_clause(table: &str) -> &'static str {
    // For tables with a composite PK (track_metadata) or a text PK
    // (settings), we need to handle ON CONFLICT differently.
    match table {
        "settings" => "ON CONFLICT (key) DO NOTHING",
        "track_metadata" => "ON CONFLICT (track_id, key) DO NOTHING",
        "album_metadata" => "ON CONFLICT (album_id, key) DO NOTHING",
        "radio_favorites" => "ON CONFLICT (title, artist) DO NOTHING",
        "favorites" => "ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
        "item_tags" => "ON CONFLICT (tag_id, item_type, item_id) DO NOTHING",
        // Cette table n'a PAS de colonne `id` : la clause par defaut
        // `ON CONFLICT (id)` ci-dessous echouerait sur elle (#3699).
        "streaming_item_tags" => "ON CONFLICT (tag_id, item_type, source, source_id) DO NOTHING",
        // Meme cas (#4806) : pas de colonne `id`, la clef est la paire.
        "streaming_hidden_items" => {
            "ON CONFLICT (profile_id, item_type, source, source_id) DO NOTHING"
        }
        // Meme cas : pas de colonne `id`, la clef primaire est l'UDN — la
        // seule identite d'un appareil UPnP qui survive a un changement de
        // port (#2219, phase 1).
        "upnp_library_sources" => "ON CONFLICT (source_key) DO NOTHING",
        "upnp_library_members" => "ON CONFLICT (source_key, track_id) DO NOTHING",
        "media_servers" => "ON CONFLICT (udn) DO NOTHING",
        // Pas de colonne `id` : la clef primaire est la paire (#4853).
        "collection_folder_items" => "ON CONFLICT (kind, collection_id) DO NOTHING",
        "album_ratings" => "ON CONFLICT (album_id, profile_id) DO NOTHING",
        "offline_cache" => "ON CONFLICT (source, source_id) DO NOTHING",
        "track_source_links" => "ON CONFLICT (track_id, service) DO NOTHING",
        // Pas de colonne `id` non plus : la clef primaire, lue dans les
        // migrations SQLite et PostgreSQL. Sans ces lignes, la clause par
        // defaut ci-dessous faisait echouer leur copie (« column "id" does not
        // exist ») et la bascule perdait en silence les albums et titres
        // masques ou bannis (#1391), les favoris de facette (#2442), les
        // arbitrages de doublons (#1276) et les appareils ignores (#1280).
        "favorite_facets" => "ON CONFLICT (profile_id, facet, value) DO NOTHING",
        "hidden_items" => "ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
        "album_distinct_pairs" => "ON CONFLICT (profile_id, album_a_id, album_b_id) DO NOTHING",
        "ignored_devices" => "ON CONFLICT (device_id) DO NOTHING",
        // For tables with BIGSERIAL PK, conflict on id
        _ => "ON CONFLICT (id) DO NOTHING",
    }
}

/// Insert a batch of rows into PG using a single multi-row INSERT.
/// Le type PostgreSQL vers lequel convertir chaque parametre de la copie,
/// colonne par colonne — `None` pour une colonne texte, qui recoit le texte
/// tel quel.
///
/// 🔴 La copie lie TOUT en texte (voir [`bind_migration_value`]). Sur la
/// base toute neuve que monte `PG_FULL_SCHEMA`, les colonnes sont TEXT et le
/// texte y entre. Mais `run_pg_migrations`, en fin de bascule, convertit les
/// `id`, `profile_id`, etc. en BIGINT : une bascule REJOUEE sur cette base
/// voyait chaque INSERT refuse — « column "id" is of type bigint but
/// expression is of type text » — et `profiles`, `radio_stations`,
/// `hidden_items`… partaient en `pg_migrate_table_skipped` (#5199, releve par #5139).
/// Un parametre texte n'a pas de conversion implicite vers BIGINT ; un
/// `$n::int8` explicite, si. Le type se lit dans le catalogue de la base
/// visee, jamais dans une liste tenue a la main.
///
/// Une colonne TEXT (premiere bascule) ne recoit aucune conversion : la
/// requete y reste celle d'avant, a l'octet pres.
async fn column_casts(
    pool: &PgPool,
    table: &str,
    columns: &[String],
) -> Result<Vec<Option<String>>, String> {
    let types: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name::text, udt_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("types des colonnes de {table} : {e}"))?;
    let types: std::collections::HashMap<String, String> = types.into_iter().collect();
    Ok(columns
        .iter()
        .map(|c| {
            types.get(c).and_then(|t| {
                let sans_conversion = matches!(t.as_str(), "text" | "varchar" | "bpchar" | "bytea")
                    || t.starts_with('_');
                let nom_sur =
                    !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                (!sans_conversion && nom_sur).then(|| t.clone())
            })
        })
        .collect())
}

async fn insert_batch(
    pool: &PgPool,
    table: &str,
    columns: &[String],
    casts: &[Option<String>],
    rows: &[Vec<SqlValue>],
    conflict_clause: &str,
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }

    let col_count = columns.len();

    // Build: INSERT INTO table (col1, col2, ...) VALUES ($1, $2, ...), ($3, $4, ...), ...
    // ON CONFLICT ... DO NOTHING
    let col_list = columns.join(", ");
    let mut sql = format!("INSERT INTO {table} ({col_list}) VALUES ");

    let mut param_idx = 1u32;
    for (row_idx, _row) in rows.iter().enumerate() {
        if row_idx > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for col_idx in 0..col_count {
            if col_idx > 0 {
                sql.push_str(", ");
            }
            sql.push('$');
            sql.push_str(&param_idx.to_string());
            if let Some(Some(cast)) = casts.get(col_idx) {
                sql.push_str("::");
                sql.push_str(cast);
            }
            param_idx += 1;
        }
        sql.push(')');
    }
    sql.push(' ');
    sql.push_str(conflict_clause);

    // Bind all values. We use the text-based approach for maximum
    // compatibility: everything goes as TEXT (PG will coerce), except
    // integers and floats which bind natively, and NULLs.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for row in rows {
        for (col_idx, val) in row.iter().enumerate() {
            query = bind_migration_value(
                query,
                val,
                table,
                columns.get(col_idx).map(|s| s.as_str()).unwrap_or("?"),
            );
        }
    }

    query
        .execute(pool)
        .await
        .map_err(|e| format!("INSERT into {table}: {e}"))?;

    Ok(())
}

/// Bind a SqlValue to a sqlx query for the migration.
/// Uses native types where possible to avoid PG type mismatches.
fn bind_migration_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    val: &SqlValue,
    _table: &str,
    _col: &str,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match val {
        SqlValue::Null
        | SqlValue::NullInt
        | SqlValue::NullText
        | SqlValue::NullReal
        | SqlValue::NullBool
        | SqlValue::NullBlob => query.bind(Option::<String>::None),
        SqlValue::Int(i) => query.bind(i.to_string()),
        SqlValue::Real(f) => query.bind(f.to_string()),
        SqlValue::Bool(b) => query.bind(if *b { "1".to_string() } else { "0".to_string() }),
        SqlValue::Text(s) => query.bind(s.clone()),
        SqlValue::Blob(b) => query.bind(b.clone()),
    }
}

/// Get column names for a SQLite table via PRAGMA table_info.
fn get_sqlite_columns(db: &SqliteDb, table: &str) -> Result<Vec<String>, String> {
    let conn = db.read_connection();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| format!("pragma table_info({table}): {e}"))?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("table_info query {table}: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("table_info collect {table}: {e}"))?;
    Ok(cols)
}

// `reset_sequences` vivait ici : après une migration qui insère des `id`
// explicites, elle repositionnait chaque séquence PostgreSQL à MAX(id)+1.
// Elle n'a plus d'appelant depuis « PG migration — TEXT PRIMARY KEY for all
// tables, remove sequences » (7b947b01), et elle ne pourrait plus servir telle
// quelle : sa liste de tables décrit le schéma BIGSERIAL d'avant, et son
// `MAX(id) + 1` sur une colonne devenue TEXT n'a pas d'opérateur en PG — les
// erreurs partaient en `pg_sequence_reset_skipped`, donc en silence.
//
// ⚠️ Il reste TROIS séquences vivantes que rien ne repositionne après une
// migration SQLite → PG : `zones_id_seq` (ici l.350), `streaming_favorites_id_seq`
// (ici l.569 et db/postgres.rs) et `queue_items_id_seq` (db/postgres.rs). Le
// correctif demande un PostgreSQL réel pour être prouvé ; il n'est PAS fait ici,
// et rebrancher cette fonction-là ne l'aurait pas fait non plus.

/// La bascule SQLite -> PostgreSQL sur un VRAI PostgreSQL, table par table.
///
/// Sautes sans `TUNE_TEST_PG_URL` ; l'etape « Bascule SQLite -> PostgreSQL,
/// clefs de conflit » de `test-postgres.yml` les execute (#5134).
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Connection, PgConnection, Row};

    /// Remplace le nom de base dans une URL `postgresql://…/nom[?…]`.
    fn url_vers_base(url: &str, base: &str) -> String {
        let (avant, apres) = match url.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (url, None),
        };
        let racine = avant.rsplit_once('/').map(|(r, _)| r).unwrap_or(avant);
        match apres {
            Some(q) => format!("{racine}/{base}?{q}"),
            None => format!("{racine}/{base}"),
        }
    }

    /// Detruit puis recree une base jetable, avec `unaccent` comme la CI, et
    /// rend son URL. `nom` est une constante de ce module, jamais une entree.
    async fn base_jetable(url: &str, nom: &str) -> String {
        let mut maintenance = PgConnection::connect(url)
            .await
            .unwrap_or_else(|e| panic!("connexion a {url} : {e}"));
        for ordre in [
            format!("DROP DATABASE IF EXISTS {nom} WITH (FORCE)"),
            format!("CREATE DATABASE {nom}"),
        ] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(ordre.clone()))
                .execute(&mut maintenance)
                .await
                .unwrap_or_else(|e| panic!("{ordre} : {e}"));
        }
        maintenance.close().await.ok();
        let cible = url_vers_base(url, nom);
        let mut c = PgConnection::connect(&cible).await.unwrap();
        sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS unaccent")
            .execute(&mut c)
            .await
            .unwrap();
        c.close().await.ok();
        cible
    }

    async fn supprimer_base(url: &str, nom: &str) {
        if let Ok(mut m) = PgConnection::connect(url).await {
            let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {nom} WITH (FORCE)"
            )))
            .execute(&mut m)
            .await;
        }
    }

    /// Les tables de `MIGRATION_TABLES` qui n'ont PAS de colonne `id` : elles
    /// ne peuvent pas tomber dans la clause par defaut `ON CONFLICT (id)`.
    /// Chacune recoit deux lignes dans la base SQLite de depart.
    const LIGNES_SQLITE: &str = "
        INSERT INTO hidden_items (profile_id, item_type, item_id, item_name, item_artist)
            VALUES (1, 'album', 11, 'Kind of Blue', 'Miles Davis'),
                   (1, 'track', 12, 'So What', 'Miles Davis');
        INSERT INTO favorite_facets (profile_id, facet, value)
            VALUES (1, 'label', 'ECM'), (1, 'label', 'Blue Note');
        INSERT INTO album_distinct_pairs (profile_id, album_a_id, album_b_id, a_name, b_name)
            VALUES (1, 21, 22, 'Live', 'Live (remaster)'),
                   (1, 23, 24, 'Best of', 'Best of II');
        INSERT INTO ignored_devices (device_id, mac, host, name, device_type)
            VALUES ('uuid:tv-salon', 'aa:bb:cc:dd:ee:01', '192.168.1.40', 'TV salon', 'upnp'),
                   ('uuid:box', 'aa:bb:cc:dd:ee:02', '192.168.1.1', 'Box', 'upnp');
    ";
    const TABLES_SANS_ID: [&str; 4] = [
        "hidden_items",
        "favorite_facets",
        "album_distinct_pairs",
        "ignored_devices",
    ];

    fn sqlite_de_depart() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db.connection()
            .lock()
            .unwrap()
            .execute_batch(LIGNES_SQLITE)
            .expect("lignes SQLite de depart");
        db
    }

    async fn compte(url: &str, table: &str) -> i64 {
        let mut c = PgConnection::connect(url).await.unwrap();
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&mut c)
                .await
                .unwrap();
        c.close().await.ok();
        n
    }

    /// Aller-retour : la bascule copie les lignes des tables sans `id`, en
    /// entier et a l'identique.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_bascule_tables_sans_id_aller_retour() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const BASE: &str = "tune_bascule_sans_id";
        let cible = base_jetable(&url, BASE).await;
        let sqlite = sqlite_de_depart();

        let r = migrate_sqlite_to_pg(&sqlite, &cible)
            .await
            .unwrap_or_else(|e| panic!("bascule : {e}"));
        let mut pertes = Vec::new();
        for table in TABLES_SANS_ID {
            let erreurs: Vec<&String> = r
                .errors
                .iter()
                .filter(|e| e.starts_with(&format!("{table}:")))
                .collect();
            let detail = r.details.iter().find(|d| d.table == table).unwrap();
            let en_pg = compte(&cible, table).await;
            if !erreurs.is_empty() || detail.skipped || detail.rows != 2 || en_pg != 2 {
                pertes.push(format!(
                    "{table} : {en_pg} ligne(s) sur 2 en PostgreSQL, erreurs {erreurs:?}"
                ));
            }
        }
        assert!(
            pertes.is_empty(),
            "lignes perdues a la bascule :\n{}",
            pertes.join("\n")
        );

        // Le contenu, pas seulement le nombre.
        let mut c = PgConnection::connect(&cible).await.unwrap();
        let masques: Vec<String> =
            sqlx::query("SELECT item_type || ':' || item_id FROM hidden_items ORDER BY 1")
                .fetch_all(&mut c)
                .await
                .unwrap()
                .iter()
                .map(|l| l.get::<String, _>(0))
                .collect();
        assert_eq!(masques, ["album:11", "track:12"]);
        let labels: Vec<String> = sqlx::query("SELECT value FROM favorite_facets ORDER BY 1")
            .fetch_all(&mut c)
            .await
            .unwrap()
            .iter()
            .map(|l| l.get::<String, _>(0))
            .collect();
        assert_eq!(labels, ["Blue Note", "ECM"]);
        c.close().await.ok();

        supprimer_base(&url, BASE).await;
    }

    /// Reprise d'une copie interrompue : la meme table recopiee sur une base
    /// qui porte deja ses lignes. C'est le cas que la clause `ON CONFLICT`
    /// sert : ni erreur, ni doublon.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_bascule_tables_sans_id_reprise_sans_doublon() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const BASE: &str = "tune_bascule_reprise";
        let cible = base_jetable(&url, BASE).await;
        let sqlite = sqlite_de_depart();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&cible)
            .await
            .unwrap();
        sqlx::raw_sql(PG_FULL_SCHEMA)
            .execute(&pool)
            .await
            .expect("PG_FULL_SCHEMA");

        let mut echecs = Vec::new();
        for table in TABLES_SANS_ID {
            for passage in ["copie", "reprise"] {
                match migrate_table(&sqlite, &pool, table).await {
                    Ok(2) => {}
                    autre => echecs.push(format!("{table} ({passage}) : {autre:?}")),
                }
            }
            let en_pg = compte(&cible, table).await;
            if en_pg != 2 {
                echecs.push(format!(
                    "{table} : {en_pg} ligne(s) en PostgreSQL au lieu de 2"
                ));
            }
        }
        pool.close().await;
        supprimer_base(&url, BASE).await;
        assert!(
            echecs.is_empty(),
            "reprise de la copie :\n{}",
            echecs.join("\n")
        );
    }

    /// Le type d'une colonne, lu dans le catalogue de la base `url`.
    async fn type_de_colonne(url: &str, table: &str, colonne: &str) -> Option<String> {
        let mut c = PgConnection::connect(url).await.unwrap();
        let t: Option<String> = sqlx::query_scalar(
            "SELECT data_type::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(colonne)
        .fetch_optional(&mut c)
        .await
        .unwrap();
        c.close().await.ok();
        t
    }

    /// Bascule REJOUEE : la seconde bascule complete, sur une base deja
    /// basculee, doit passer — sans nouvelle erreur, sans rien perdre.
    ///
    /// #5199, releve par #5139 : la seconde passe echouait a la creation du schema,
    /// « column "id" is of type bigint but expression is of type text ». Le
    /// bloc de copie `'lq_' || id` de `PG_FULL_SCHEMA` ne se gardait que sur
    /// « `queue_items` est vide » — vrai ici, la file SQLite l'etant — alors
    /// que la premiere bascule avait converti son `id` en BIGINT.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_rejeu_bascule_deux_fois() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const BASE: &str = "tune_bascule_rejeu";
        let cible = base_jetable(&url, BASE).await;
        let sqlite = sqlite_de_depart();

        let premiere = migrate_sqlite_to_pg(&sqlite, &cible).await;
        // La condition du defaut : apres une bascule, `queue_items` est vide
        // et son `id` n'est plus TEXT. Sans elle, ce temoin ne garderait rien.
        let type_id = type_de_colonne(&cible, "queue_items", "id").await;
        let file = compte(&cible, "queue_items").await;
        let seconde = migrate_sqlite_to_pg(&sqlite, &cible).await;
        let mut restes = Vec::new();
        for table in TABLES_SANS_ID {
            restes.push((table, compte(&cible, table).await));
        }
        supprimer_base(&url, BASE).await;

        let premiere = premiere.unwrap_or_else(|e| panic!("premiere bascule : {e}"));
        assert_eq!(
            type_id.as_deref(),
            Some("bigint"),
            "apres la premiere bascule, queue_items.id devait etre BIGINT (migration 012)"
        );
        assert_eq!(
            file, 0,
            "queue_items devait etre vide apres la premiere bascule"
        );

        let seconde = seconde.unwrap_or_else(|e| {
            panic!("la bascule ne se rejoue pas sur une base deja basculee (#5199) : {e}")
        });
        let nouvelles: Vec<&String> = seconde
            .errors
            .iter()
            .filter(|e| !premiere.errors.contains(e))
            .collect();
        assert!(
            nouvelles.is_empty(),
            "le rejeu de la bascule ajoute des erreurs :\n{nouvelles:#?}"
        );
        for (table, n) in restes {
            assert_eq!(n, 2, "{table} : {n} ligne(s) apres le rejeu au lieu de 2");
        }
    }

    /// Chaque clause de `conflict_clause` doit nommer EXACTEMENT les colonnes
    /// d'une clef primaire ou d'un index unique de sa table, lus dans le
    /// catalogue du PostgreSQL que `PG_FULL_SCHEMA` vient de monter. Garde
    /// toute table ajoutee plus tard a `MIGRATION_TABLES` sans sa clause.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_bascule_chaque_clause_vise_une_clef() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const BASE: &str = "tune_bascule_clefs";
        let cible = base_jetable(&url, BASE).await;
        let mut c = PgConnection::connect(&cible).await.unwrap();
        sqlx::raw_sql(PG_FULL_SCHEMA)
            .execute(&mut c)
            .await
            .expect("PG_FULL_SCHEMA");

        let mut ecarts = Vec::new();
        for table in MIGRATION_TABLES {
            let clause = conflict_clause(table);
            let mut visees: Vec<String> = clause
                .split_once('(')
                .and_then(|(_, r)| r.split_once(')'))
                .map(|(cols, _)| cols.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();
            visees.sort();

            let clefs: Vec<Vec<String>> = sqlx::query(
                "SELECT array_agg(a.attname::text ORDER BY a.attname::text) \
                 FROM pg_index i \
                 JOIN pg_class t ON t.oid = i.indrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ANY(i.indkey) \
                 WHERE n.nspname = 'public' AND t.relname = $1 AND i.indisunique \
                 GROUP BY i.indexrelid",
            )
            .bind(*table)
            .fetch_all(&mut c)
            .await
            .unwrap()
            .iter()
            .map(|l| l.get::<Vec<String>, _>(0))
            .collect();

            if !clefs.contains(&visees) {
                ecarts.push(format!("{table} : « {clause} », clefs reelles {clefs:?}"));
            }
        }
        c.close().await.ok();
        supprimer_base(&url, BASE).await;
        assert!(
            ecarts.is_empty(),
            "clause ON CONFLICT sans clef correspondante — la bascule perdrait ces tables :\n{}",
            ecarts.join("\n")
        );
    }
}
