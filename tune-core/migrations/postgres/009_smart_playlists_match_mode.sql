-- 009_smart_playlists_match_mode.sql
--
-- The SQLite schema added `match_mode` to smart_playlists (a later ALTER),
-- but the PG side never did — 005 created the table without it. The server
-- query selects `match_mode`, so on a PostgreSQL backend GET
-- /api/v1/library/smart-playlists failed with:
--   pg query_many: column "match_mode" does not exist
-- (observed on the .15 prod server; the iOS remote client then showed no
-- smart playlists at all).
--
-- Idempotent (ADD COLUMN IF NOT EXISTS).

BEGIN;

ALTER TABLE smart_playlists ADD COLUMN IF NOT EXISTS match_mode TEXT NOT NULL DEFAULT 'all';

INSERT INTO schema_version (version, name) VALUES (9, 'smart_playlists_match_mode')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
