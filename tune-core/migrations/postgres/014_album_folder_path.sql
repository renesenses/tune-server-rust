-- A folder on disk is what says "these files are one release". Storing it makes
-- album identity explicit instead of inferred from title + quality tier: an
-- edition whose discs differ in sample rate stays one album, and two separate
-- rips of the same album stay two.
--
-- NULL on every pre-existing row until a rescan; the lookup falls back to
-- title + artist then, so an un-rescanned library behaves exactly as before.

ALTER TABLE albums ADD COLUMN IF NOT EXISTS folder_path TEXT;

CREATE INDEX IF NOT EXISTS idx_albums_folder_path
    ON albums (folder_path)
    WHERE folder_path IS NOT NULL;
