-- 007_podcast_subscriptions.sql
--
-- The podcast_subscriptions table was created by SQLite migrations with
-- AUTOINCREMENT, which PG silently ignores — leaving id as a plain INTEGER
-- with no auto-generation. Fix: recreate with SERIAL.

BEGIN;

-- Drop the broken table (no user data worth preserving) and recreate properly.
DROP TABLE IF EXISTS podcast_subscriptions;

CREATE TABLE podcast_subscriptions (
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

COMMIT;
