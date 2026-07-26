-- 007_podcast_subscriptions.sql
--
-- The podcast_subscriptions table was created by SQLite migrations with
-- AUTOINCREMENT, which PG silently ignores — leaving id as a plain INTEGER
-- with no auto-generation. Fix: recreate with SERIAL.
--
-- The DROP is guarded on the broken condition (id column without a default):
-- this script must be safe to REPLAY on a healthy database, because the
-- migration runner replays all scripts when it drops the data-migration
-- sentinel (version 99). An unconditional DROP wiped the user's podcast
-- subscriptions on that replay.

BEGIN;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'podcast_subscriptions'
          AND column_name = 'id'
          AND column_default IS NULL
    ) THEN
        -- Broken AUTOINCREMENT-less table: no auto-generated ids, inserts
        -- fail — nothing worth preserving, recreate properly.
        DROP TABLE podcast_subscriptions;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS podcast_subscriptions (
    id SERIAL PRIMARY KEY,
    feed_url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    author TEXT,
    image_url TEXT,
    description TEXT,
    last_checked TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Also ensure radio_favorites exists with SERIAL id
CREATE TABLE IF NOT EXISTS radio_favorites (
    id SERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT,
    station_name TEXT,
    cover_url TEXT,
    stream_url TEXT,
    saved_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Record the version. Without this the runner's MAX(version) stayed at 6,
-- so this migration re-ran on every boot — dropping podcast_subscriptions
-- each time until a later migration bumped the max past 7.
INSERT INTO schema_version (version, name) VALUES (7, 'podcast_subscriptions')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
