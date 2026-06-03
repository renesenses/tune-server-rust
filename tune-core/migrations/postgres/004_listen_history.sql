-- 004_listen_history.sql
--
-- Adds the `listen_history` table to PG. This table backs
-- `history_repo` (record / recent / dashboard / full_dashboard).
-- It's runtime-created on SQLite via the migrations runner, but
-- wasn't in `001_initial_schema.sql`. This migration plus the index
-- on `listened_at` brings PG to parity for the history features.

BEGIN;

CREATE TABLE IF NOT EXISTS listen_history (
    id BIGSERIAL PRIMARY KEY,
    track_id BIGINT REFERENCES tracks(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    artist_name TEXT,
    album_title TEXT,
    source TEXT NOT NULL DEFAULT 'local',
    duration_ms BIGINT NOT NULL DEFAULT 0,
    listened_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    zone_id BIGINT REFERENCES zones(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_listen_history_listened_at
    ON listen_history(listened_at DESC);
CREATE INDEX IF NOT EXISTS idx_listen_history_zone_id
    ON listen_history(zone_id);

INSERT INTO schema_version (version, name) VALUES (4, 'listen_history')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
