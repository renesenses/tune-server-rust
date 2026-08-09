-- 021_track_cover_path.sql
--
-- Adds `tracks.cover_path`: the cover embedded in THIS file, as opposed to
-- `albums.cover_path` which stands for the whole album.
--
-- Why a per-track cover at all: the scanner treats a folder holding several
-- artists AND several album tags as a hand-made compilation and files every
-- file under one album named after the folder. That is right for a real
-- compilation, but a folder of unrelated files gets the same treatment — and
-- the first file carrying artwork then lent its cover to all the others
-- (forum #1312: a WAV by one artist showing another artist's sleeve).
--
-- The album cover stays the norm. Reads use
-- COALESCE(t.cover_path, al.cover_path), so a track without its own artwork
-- keeps showing its album's, exactly as before.
--
-- Idempotent: ADD COLUMN IF NOT EXISTS is safe to re-run.

BEGIN;

ALTER TABLE tracks
    ADD COLUMN IF NOT EXISTS cover_path TEXT;

INSERT INTO schema_version (version, name) VALUES (21, 'track_cover_path')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
