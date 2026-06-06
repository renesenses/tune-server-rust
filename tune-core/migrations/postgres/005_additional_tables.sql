-- 005_additional_tables.sql
--
-- Creates the tables from SQLite migrations 2-22 that were missing in
-- the PG schema. Prerequisite for `tune db migrate-to-postgres` to
-- copy rows for these tables.
--
-- Tables added:
--   radio_stations      (SQLite migration 2)
--   settings            (SQLite migration 4)
--   bookmarks           (SQLite migration 5)
--   profiles            (SQLite migration 6)
--   favorites           (SQLite migration 6)
--   tags                (SQLite migration 6)
--   item_tags           (SQLite migration 6)
--   album_ratings       (SQLite migration 6)
--   smart_playlists     (SQLite migration 6)
--   radio_favorites     (SQLite migration 8)
--   track_source_links  (SQLite migration 22)
--
-- Usage:
--   psql -h host -U user -d dbname -f 005_additional_tables.sql
--
-- Idempotent (IF NOT EXISTS everywhere).

BEGIN;

-- ── radio_stations (migration 2) ──────────────────────────────────────

CREATE TABLE IF NOT EXISTS radio_stations (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    homepage TEXT,
    logo_url TEXT,
    country TEXT,
    language TEXT,
    genre TEXT,
    codec TEXT,
    bitrate INTEGER,
    is_favorite SMALLINT DEFAULT 0,
    last_played TEXT,
    play_count INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_radio_stations_favorite
    ON radio_stations(is_favorite);

-- ── settings (migration 4) ────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- ── bookmarks (migration 5) ──────────────────────────────────────────

CREATE TABLE IF NOT EXISTS bookmarks (
    id BIGSERIAL PRIMARY KEY,
    track_id BIGINT REFERENCES tracks(id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    label TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE INDEX IF NOT EXISTS idx_bookmarks_track_id ON bookmarks(track_id);

-- ── profiles (migration 6) ───────────────────────────────────────────

CREATE TABLE IF NOT EXISTS profiles (
    id BIGSERIAL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    avatar_path TEXT,
    password_hash TEXT,
    is_admin SMALLINT DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- ── favorites (migration 6) ─────────────────────────────────────────

CREATE TABLE IF NOT EXISTS favorites (
    id BIGSERIAL PRIMARY KEY,
    profile_id BIGINT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id BIGINT NOT NULL,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(profile_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_favorites_profile
    ON favorites(profile_id, item_type);

-- ── tags (migration 6) ──────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS tags (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT DEFAULT '#808080'
);

-- ── item_tags (migration 6) ─────────────────────────────────────────

CREATE TABLE IF NOT EXISTS item_tags (
    id BIGSERIAL PRIMARY KEY,
    tag_id BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    item_type TEXT NOT NULL,
    item_id BIGINT NOT NULL,
    UNIQUE(tag_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_item_tags_item
    ON item_tags(item_type, item_id);

-- ── album_ratings (migration 6) ─────────────────────────────────────

CREATE TABLE IF NOT EXISTS album_ratings (
    id BIGSERIAL PRIMARY KEY,
    album_id BIGINT NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    profile_id BIGINT NOT NULL DEFAULT 1,
    rating INTEGER NOT NULL CHECK(rating >= 1 AND rating <= 5),
    note TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(album_id, profile_id)
);
CREATE INDEX IF NOT EXISTS idx_album_ratings_album
    ON album_ratings(album_id);

-- ── smart_playlists (migration 6) ───────────────────────────────────

CREATE TABLE IF NOT EXISTS smart_playlists (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    sort_by TEXT DEFAULT 'title',
    sort_order TEXT DEFAULT 'asc',
    max_tracks INTEGER,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- ── radio_favorites (migration 8) ───────────────────────────────────

CREATE TABLE IF NOT EXISTS radio_favorites (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT DEFAULT '',
    station_name TEXT DEFAULT '',
    cover_url TEXT,
    stream_url TEXT,
    saved_at BIGINT,
    UNIQUE(title, artist)
);

-- ── track_source_links (migration 22) ───────────────────────────────

CREATE TABLE IF NOT EXISTS track_source_links (
    id BIGSERIAL PRIMARY KEY,
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    service_track_id TEXT NOT NULL,
    confidence DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    match_method TEXT,
    linked_at TEXT DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    UNIQUE(track_id, service)
);
CREATE INDEX IF NOT EXISTS idx_track_source_links_track
    ON track_source_links(track_id);
CREATE INDEX IF NOT EXISTS idx_track_source_links_service
    ON track_source_links(service);

-- ── Seed default profile (mirrors SQLite migration 6) ───────────────

INSERT INTO profiles (id, username, display_name, is_admin)
    VALUES (1, 'default', 'Default', 1)
    ON CONFLICT (id) DO NOTHING;

-- ── Track version ───────────────────────────────────────────────────

INSERT INTO schema_version (version, name) VALUES (5, 'additional_tables')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
