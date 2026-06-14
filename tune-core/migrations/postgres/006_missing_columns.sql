-- 006_missing_columns.sql
--
-- Adds columns that exist in the SQLite schema (added by programmatic
-- migrations in run_migrations()) but are missing from the PG schema
-- files 001-005.
--
-- Uses ADD COLUMN IF NOT EXISTS (PG 9.6+) for idempotency.

BEGIN;

-- ── tracks: extra metadata columns ──────────────────────────────────

ALTER TABLE tracks ADD COLUMN IF NOT EXISTS waveform_json TEXT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS acoustid_fingerprint TEXT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS acoustid_confidence DOUBLE PRECISION;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS trailing_silence_ms BIGINT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS synced_lyrics TEXT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS comments TEXT;

-- ── zones: DSP, playback position, max sample rate ──────────────────

ALTER TABLE zones ADD COLUMN IF NOT EXISTS dsp_preset_id BIGINT;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dsp_enabled SMALLINT DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS max_sample_rate INTEGER;

-- ── profiles: email + argon2 password ───────────────────────────────

ALTER TABLE profiles ADD COLUMN IF NOT EXISTS email TEXT;
ALTER TABLE profiles ADD COLUMN IF NOT EXISTS password_hash_v2 TEXT;

-- ── smart_collections: extra display columns ────────────────────────

ALTER TABLE smart_collections ADD COLUMN IF NOT EXISTS description TEXT;
ALTER TABLE smart_collections ADD COLUMN IF NOT EXISTS icon TEXT;
ALTER TABLE smart_collections ADD COLUMN IF NOT EXISTS color TEXT;

-- ── listen_history: cover URL ───────────────────────────────────────

ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS cover_url TEXT;

-- ── streaming_queue table ───────────────────────────────────────────

CREATE TABLE IF NOT EXISTS streaming_queue (
    id BIGSERIAL PRIMARY KEY,
    zone_id BIGINT NOT NULL,
    position INTEGER NOT NULL,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    duration_ms BIGINT DEFAULT 0
);

-- ── track_metadata table (SQLite migration 34) ─────────────────────

CREATE TABLE IF NOT EXISTS track_metadata (
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (track_id, key)
);
CREATE INDEX IF NOT EXISTS idx_track_metadata_key ON track_metadata(key);

-- ── Track version ───────────────────────────────────────────────────

INSERT INTO schema_version (version, name) VALUES (6, 'missing_columns')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
