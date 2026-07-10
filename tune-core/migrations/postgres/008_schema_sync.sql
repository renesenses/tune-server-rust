-- 008_schema_sync.sql
--
-- Brings the PostgreSQL schema to parity with the current SQLite schema.
-- Migrations 001-007 covered 24 tables (plus smart_collections, whose
-- base table is now created in 006 before its ALTERs); the SQLite side
-- has since grown 10 more tables that were never ported. Without them
-- `tune db migrate-to-postgres` skips their rows and the PG backend
-- cannot back those features. Column definitions mirror the live SQLite
-- DDL; types
-- follow the same conventions as 005 (BIGSERIAL id, BIGINT foreign keys,
-- SMALLINT boolean flags, DOUBLE PRECISION for REAL, TEXT timestamps via
-- to_char(now())).
--
-- Tables added:
--   streaming_auth       (per-service auth blob)
--   alarms               (wake/scheduled playback)
--   lyrics_cache         (lrclib/synced lyrics)
--   metadata_suggestions (enrichment review queue)
--   network_mounts       (SMB/NFS library mounts)
--   offline_cache        (downloaded streaming tracks)
--   queue_items          (per-zone playback queue)
--   sync_links           (local <-> service playlist links)
--   sync_link_snapshots  (sync diff snapshots, FK -> sync_links)
--   sync_changelog       (entity change journal)
--
-- Idempotent (IF NOT EXISTS everywhere).

BEGIN;

-- ── streaming_auth ───────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS streaming_auth (
    service TEXT PRIMARY KEY,
    token_data TEXT NOT NULL
);

-- ── alarms ───────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS alarms (
    id BIGSERIAL PRIMARY KEY,
    zone_id BIGINT REFERENCES zones(id) ON DELETE CASCADE,
    time TEXT NOT NULL,
    enabled SMALLINT DEFAULT 1,
    days TEXT DEFAULT '1,2,3,4,5,6,7',
    source_type TEXT DEFAULT 'playlist',
    source_id BIGINT,
    volume DOUBLE PRECISION DEFAULT 0.3,
    fade_in_seconds INTEGER DEFAULT 30,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    name TEXT DEFAULT 'Alarm',
    one_shot SMALLINT DEFAULT 0,
    skip_holidays SMALLINT DEFAULT 0,
    source_name TEXT,
    fade_duration_s INTEGER DEFAULT 60,
    last_fired_at TEXT,
    days_of_week TEXT DEFAULT '1111111',
    multi_zone_ids TEXT
);
CREATE INDEX IF NOT EXISTS idx_alarms_zone ON alarms(zone_id);

-- ── lyrics_cache ─────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS lyrics_cache (
    track_id BIGINT PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    synced_lyrics TEXT,
    plain_lyrics TEXT,
    source TEXT NOT NULL DEFAULT 'lrclib',
    fetched_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- ── metadata_suggestions ─────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS metadata_suggestions (
    id BIGSERIAL PRIMARY KEY,
    track_id BIGINT,
    album_id BIGINT,
    field TEXT NOT NULL,
    suggested_value TEXT NOT NULL,
    source TEXT NOT NULL,
    confidence DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE INDEX IF NOT EXISTS idx_ms_track ON metadata_suggestions(track_id);
CREATE INDEX IF NOT EXISTS idx_ms_album ON metadata_suggestions(album_id);
CREATE INDEX IF NOT EXISTS idx_ms_status ON metadata_suggestions(status);

-- ── network_mounts ───────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS network_mounts (
    id BIGSERIAL PRIMARY KEY,
    mount_type TEXT NOT NULL DEFAULT 'smb',
    server TEXT NOT NULL,
    share TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    username TEXT,
    password TEXT,
    active SMALLINT DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- ── offline_cache ────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS offline_cache (
    id BIGSERIAL PRIMARY KEY,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    track_title TEXT,
    artist_name TEXT,
    album_title TEXT,
    file_path TEXT,
    file_size BIGINT,
    duration_ms INTEGER,
    quality TEXT,
    downloaded_at TEXT DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    expires_at TEXT,
    status TEXT DEFAULT 'pending',
    error TEXT,
    UNIQUE(source, source_id)
);
CREATE INDEX IF NOT EXISTS idx_offline_cache_status ON offline_cache(status);

-- ── queue_items ──────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS queue_items (
    id BIGSERIAL PRIMARY KEY,
    zone_id BIGINT NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    position INTEGER NOT NULL DEFAULT 0,
    is_current SMALLINT DEFAULT 0,
    track_id BIGINT REFERENCES tracks(id) ON DELETE CASCADE,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    duration_ms INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_queue_items_zone_id ON queue_items(zone_id);

-- (smart_collections is created by migration 006, before its ALTERs.)

-- ── sync_links (must precede sync_link_snapshots) ────────────────────

CREATE TABLE IF NOT EXISTS sync_links (
    id BIGSERIAL PRIMARY KEY,
    local_playlist_id BIGINT NOT NULL,
    service TEXT NOT NULL,
    remote_playlist_id TEXT NOT NULL,
    direction TEXT NOT NULL DEFAULT '"bidirectional"',
    last_synced TEXT,
    created_at TEXT DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

-- ── sync_link_snapshots (FK -> sync_links) ───────────────────────────

CREATE TABLE IF NOT EXISTS sync_link_snapshots (
    id BIGSERIAL PRIMARY KEY,
    playlist_link_id BIGINT NOT NULL REFERENCES sync_links(id) ON DELETE CASCADE,
    side TEXT NOT NULL,
    tracks_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sync_snapshots_link
    ON sync_link_snapshots(playlist_link_id, side);

-- ── sync_changelog ───────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sync_changelog (
    id BIGSERIAL PRIMARY KEY,
    entity_type TEXT NOT NULL,
    entity_id BIGINT NOT NULL,
    action TEXT NOT NULL,
    changed_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS'),
    synced SMALLINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sync_changelog_unsynced
    ON sync_changelog(synced, changed_at);
CREATE INDEX IF NOT EXISTS idx_sync_changelog_entity
    ON sync_changelog(entity_type, entity_id);

-- ── Track version ────────────────────────────────────────────────────

INSERT INTO schema_version (version, name) VALUES (8, 'schema_sync')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
