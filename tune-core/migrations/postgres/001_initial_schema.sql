-- Tune initial schema for PostgreSQL.
--
-- This is the PG-flavored translation of the SQLite CORE_SCHEMA in
-- tune-core/src/db/sqlite.rs. It bootstraps a fresh Postgres database
-- with the core tables (artists, albums, tracks, track_credits,
-- playlists, playlist_tracks, zones, play_queue) and their indexes.
--
-- Differences vs SQLite:
--   - INTEGER PRIMARY KEY AUTOINCREMENT → BIGSERIAL PRIMARY KEY
--   - REAL                              → DOUBLE PRECISION
--   - INTEGER (for booleans)            → SMALLINT (keep 0/1 semantics
--                                         for back-compat with SQLite
--                                         data dumps; promotion to
--                                         BOOLEAN is a later migration)
--
-- Usage on a fresh PG instance:
--     psql -h host -U user -d dbname -f 001_initial_schema.sql
--
-- The FTS5 virtual tables and their triggers from the SQLite schema
-- are NOT included here: PG uses tsvector + GIN indexes instead, and
-- that material lives in 002_fts_tsvector.sql (phase 4 of PG
-- roadmap, not yet written).

BEGIN;

CREATE TABLE IF NOT EXISTS artists (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    sort_name TEXT,
    musicbrainz_id TEXT,
    discogs_id TEXT,
    bio TEXT,
    image_path TEXT,
    image_source TEXT
);

CREATE TABLE IF NOT EXISTS albums (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    artist_id BIGINT REFERENCES artists(id),
    year INTEGER,
    original_year INTEGER,
    genre TEXT,
    genres TEXT,
    disc_count INTEGER DEFAULT 1,
    track_count INTEGER DEFAULT 0,
    cover_path TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    label TEXT,
    catalog_number TEXT,
    barcode TEXT,
    format TEXT,
    sample_rate INTEGER,
    bit_depth INTEGER,
    bio TEXT,
    musicbrainz_release_id TEXT,
    musicbrainz_release_group_id TEXT,
    release_date TEXT,
    original_date TEXT
);

CREATE TABLE IF NOT EXISTS tracks (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    album_id BIGINT REFERENCES albums(id),
    artist_id BIGINT REFERENCES artists(id),
    album_artist TEXT,
    disc_number INTEGER DEFAULT 1,
    disc_subtitle TEXT,
    track_number INTEGER DEFAULT 0,
    duration_ms BIGINT DEFAULT 0,
    file_path TEXT UNIQUE,
    format TEXT,
    sample_rate INTEGER,
    bit_depth INTEGER,
    channels INTEGER DEFAULT 2,
    file_mtime DOUBLE PRECISION,
    file_size BIGINT,
    audio_hash TEXT,
    source TEXT DEFAULT 'local',
    source_id TEXT,
    isrc TEXT,
    genre TEXT,
    genres TEXT,
    composer TEXT,
    year INTEGER,
    bpm DOUBLE PRECISION,
    label TEXT,
    musicbrainz_recording_id TEXT
);

CREATE TABLE IF NOT EXISTS track_credits (
    id BIGSERIAL PRIMARY KEY,
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_id BIGINT REFERENCES artists(id),
    artist_name TEXT NOT NULL,
    role TEXT DEFAULT 'performer',
    instrument TEXT,
    position INTEGER DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_tracks_file_path ON tracks(file_path);
CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_audio_hash ON tracks(audio_hash);
CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id);

CREATE TABLE IF NOT EXISTS playlists (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT
);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    id BIGSERIAL PRIMARY KEY,
    playlist_id BIGINT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS zones (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    output_type TEXT,
    output_device_id TEXT,
    volume INTEGER DEFAULT 50,
    muted SMALLINT DEFAULT 0,
    online SMALLINT DEFAULT 1,
    gapless_enabled SMALLINT DEFAULT 1,
    group_id TEXT,
    sync_delay_ms INTEGER NOT NULL DEFAULT 0,
    last_position_ms BIGINT NOT NULL DEFAULT 0,
    last_track_id BIGINT,
    last_track_source TEXT,
    last_track_source_id TEXT
);

CREATE TABLE IF NOT EXISTS play_queue (
    id BIGSERIAL PRIMARY KEY,
    zone_id BIGINT NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    is_current SMALLINT DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_track_credits_track_id ON track_credits(track_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist_id ON track_credits(artist_id);
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_playlist_id ON playlist_tracks(playlist_id);
CREATE INDEX IF NOT EXISTS idx_play_queue_zone_id ON play_queue(zone_id);

-- Migration tracking table (records which migrations have been
-- applied). Mirrors the SQLite schema_version table; the SQL
-- migrations runner inserts here when it completes a script.
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TIMESTAMPTZ DEFAULT now(),
    name TEXT NOT NULL
);

INSERT INTO schema_version (version, name) VALUES (1, 'initial_schema')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
