-- Tune full-text search for PostgreSQL — tsvector + GIN equivalent
-- of the SQLite FTS5 virtual tables in 001_initial_schema.sql.
--
-- Translation map (SQLite FTS5 → PG):
--   CREATE VIRTUAL TABLE … USING fts5(…)  → tsvector column +
--                                            GIN index on base table
--   tokenize='unicode61 remove_diacritics 2' → simple dictionary +
--                                               unaccent extension
--   tracks_fts MATCH 'query*'             → search_tsv @@
--                                            to_tsquery('simple',
--                                            unaccent('query:*'))
--
-- Search SQL equivalence (used by the repos in phase 4):
--   -- SQLite
--   SELECT … FROM tracks t WHERE t.id IN
--     (SELECT rowid FROM tracks_fts WHERE tracks_fts MATCH ?);
--   -- Postgres
--   SELECT … FROM tracks t WHERE t.search_tsv @@
--     to_tsquery('simple', unaccent(?));
--
-- Tokenization is accent-insensitive via the unaccent extension,
-- which is bundled with PostgreSQL 16 (contrib package, no extra
-- install on docker postgres:16-alpine). The 'simple' dictionary
-- avoids stemming so partial-prefix search (`miles:*`) matches the
-- same set of rows as SQLite FTS5's `miles*`.
--
-- Run after 001_initial_schema.sql:
--     psql … -f 002_fts_tsvector.sql

BEGIN;

CREATE EXTENSION IF NOT EXISTS unaccent;

-- ─── artists ──────────────────────────────────────────────────────────
ALTER TABLE artists ADD COLUMN IF NOT EXISTS search_tsv tsvector;

CREATE OR REPLACE FUNCTION artists_search_tsv_refresh()
RETURNS trigger AS $$
BEGIN
    NEW.search_tsv :=
        to_tsvector('simple', unaccent(COALESCE(NEW.name, '')))
        || to_tsvector('simple', unaccent(COALESCE(NEW.sort_name, '')));
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS artists_search_tsv_trg ON artists;
CREATE TRIGGER artists_search_tsv_trg
    BEFORE INSERT OR UPDATE OF name, sort_name ON artists
    FOR EACH ROW EXECUTE FUNCTION artists_search_tsv_refresh();

CREATE INDEX IF NOT EXISTS idx_artists_search_tsv
    ON artists USING GIN (search_tsv);

-- Backfill existing rows.
UPDATE artists SET name = name;

-- ─── albums ───────────────────────────────────────────────────────────
ALTER TABLE albums ADD COLUMN IF NOT EXISTS search_tsv tsvector;

CREATE OR REPLACE FUNCTION albums_search_tsv_refresh()
RETURNS trigger AS $$
DECLARE
    artist_name TEXT;
BEGIN
    SELECT name INTO artist_name FROM artists WHERE id = NEW.artist_id;
    NEW.search_tsv :=
        to_tsvector('simple', unaccent(COALESCE(NEW.title, '')))
        || to_tsvector('simple', unaccent(COALESCE(artist_name, '')))
        || to_tsvector('simple', unaccent(COALESCE(NEW.genre, '')));
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS albums_search_tsv_trg ON albums;
CREATE TRIGGER albums_search_tsv_trg
    BEFORE INSERT OR UPDATE OF title, artist_id, genre ON albums
    FOR EACH ROW EXECUTE FUNCTION albums_search_tsv_refresh();

CREATE INDEX IF NOT EXISTS idx_albums_search_tsv
    ON albums USING GIN (search_tsv);

-- When an artist is renamed, refresh the tsvector of every album that
-- references it. SQLite FTS5 keeps tracks_fts/albums_fts in sync via
-- separate triggers on the artists table; PG mirrors that behaviour
-- with a touch-update that re-fires the BEFORE UPDATE trigger above.
CREATE OR REPLACE FUNCTION artists_propagate_to_albums()
RETURNS trigger AS $$
BEGIN
    IF NEW.name IS DISTINCT FROM OLD.name THEN
        UPDATE albums SET artist_id = artist_id WHERE artist_id = NEW.id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS artists_propagate_to_albums_trg ON artists;
CREATE TRIGGER artists_propagate_to_albums_trg
    AFTER UPDATE OF name ON artists
    FOR EACH ROW EXECUTE FUNCTION artists_propagate_to_albums();

-- Backfill existing rows.
UPDATE albums SET title = title;

-- ─── tracks ───────────────────────────────────────────────────────────
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS search_tsv tsvector;

CREATE OR REPLACE FUNCTION tracks_search_tsv_refresh()
RETURNS trigger AS $$
DECLARE
    artist_name TEXT;
    album_title TEXT;
BEGIN
    SELECT name INTO artist_name FROM artists WHERE id = NEW.artist_id;
    SELECT title INTO album_title FROM albums WHERE id = NEW.album_id;
    NEW.search_tsv :=
        to_tsvector('simple', unaccent(COALESCE(NEW.title, '')))
        || to_tsvector('simple', unaccent(COALESCE(artist_name, '')))
        || to_tsvector('simple', unaccent(COALESCE(album_title, '')))
        || to_tsvector('simple', unaccent(COALESCE(NEW.genre, '')))
        || to_tsvector('simple', unaccent(COALESCE(NEW.composer, '')));
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS tracks_search_tsv_trg ON tracks;
CREATE TRIGGER tracks_search_tsv_trg
    BEFORE INSERT OR UPDATE OF title, album_id, artist_id, genre, composer ON tracks
    FOR EACH ROW EXECUTE FUNCTION tracks_search_tsv_refresh();

CREATE INDEX IF NOT EXISTS idx_tracks_search_tsv
    ON tracks USING GIN (search_tsv);

-- Propagate artist/album renames into the dependent tracks_fts.
CREATE OR REPLACE FUNCTION artists_propagate_to_tracks()
RETURNS trigger AS $$
BEGIN
    IF NEW.name IS DISTINCT FROM OLD.name THEN
        UPDATE tracks SET artist_id = artist_id WHERE artist_id = NEW.id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS artists_propagate_to_tracks_trg ON artists;
CREATE TRIGGER artists_propagate_to_tracks_trg
    AFTER UPDATE OF name ON artists
    FOR EACH ROW EXECUTE FUNCTION artists_propagate_to_tracks();

CREATE OR REPLACE FUNCTION albums_propagate_to_tracks()
RETURNS trigger AS $$
BEGIN
    IF NEW.title IS DISTINCT FROM OLD.title THEN
        UPDATE tracks SET album_id = album_id WHERE album_id = NEW.id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS albums_propagate_to_tracks_trg ON albums;
CREATE TRIGGER albums_propagate_to_tracks_trg
    AFTER UPDATE OF title ON albums
    FOR EACH ROW EXECUTE FUNCTION albums_propagate_to_tracks();

-- Backfill existing rows.
UPDATE tracks SET title = title;

-- Record migration.
INSERT INTO schema_version (version, name) VALUES (2, 'fts_tsvector')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
