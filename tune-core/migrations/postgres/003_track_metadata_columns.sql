-- 003_track_metadata_columns.sql
--
-- Adds the metadata columns to `tracks` that exist in the SQLite
-- schema (via runtime ALTERs from the legacy code path) but are
-- not yet in `001_initial_schema.sql`.
--
-- These columns are touched by:
--   - track_repo::get_synced_lyrics / set_synced_lyrics
--   - track_repo::get_trailing_silence / set_trailing_silence
--   - track_repo::get_waveform / set_waveform
--   - track_repo::set_acoustid
--   - track_repo::list_unidentified (filters on acoustid_fingerprint)
--
-- All 11 of these methods currently fall through `sqlite_legacy` on
-- the `Arc<dyn DbBackend>` port (cf. commit 1d9e392 — track_repo
-- partial port + docs/PORTING-TRACK-REPO-PLAN.md Group D). Landing
-- this migration is the prerequisite for porting them through the
-- DbBackend trait.
--
-- Usage on a PG instance already at version 2 (post-FTS):
--     psql -h host -U user -d dbname -f 003_track_metadata_columns.sql
--
-- Idempotent: ADD COLUMN IF NOT EXISTS is safe to re-run.

BEGIN;

ALTER TABLE tracks
    ADD COLUMN IF NOT EXISTS synced_lyrics TEXT,
    ADD COLUMN IF NOT EXISTS trailing_silence_ms BIGINT,
    ADD COLUMN IF NOT EXISTS waveform_json TEXT,
    ADD COLUMN IF NOT EXISTS acoustid_fingerprint TEXT,
    ADD COLUMN IF NOT EXISTS acoustid_confidence DOUBLE PRECISION;

-- Index helping `list_unidentified` (which filters tracks lacking
-- an acoustid fingerprint).
CREATE INDEX IF NOT EXISTS idx_tracks_acoustid_fingerprint
    ON tracks(acoustid_fingerprint)
    WHERE acoustid_fingerprint IS NOT NULL;

INSERT INTO schema_version (version, name) VALUES (3, 'track_metadata_columns')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
