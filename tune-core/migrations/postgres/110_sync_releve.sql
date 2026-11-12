-- 110_sync_releve.sql
--
-- #8239 : jumelle de la migration SQLite 144 (même DDL,
-- tune-core/migrations/sync_releve.sql). Idempotent.

BEGIN;

CREATE TABLE IF NOT EXISTS sync_releve_pistes (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 album TEXT,
 album_id BIGINT,
 format TEXT,
 sample_rate BIGINT,
 bit_depth BIGINT,
 duration_ms BIGINT,
 genre TEXT,
 track_number BIGINT,
 disc_number BIGINT,
 source TEXT,
 source_id TEXT,
 isrc TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_pistes_neuf (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 album TEXT,
 album_id BIGINT,
 format TEXT,
 sample_rate BIGINT,
 bit_depth BIGINT,
 duration_ms BIGINT,
 genre TEXT,
 track_number BIGINT,
 disc_number BIGINT,
 source TEXT,
 source_id TEXT,
 isrc TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_albums (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 annee BIGINT,
 genre TEXT,
 source TEXT,
 nb_pistes BIGINT,
 mbid_groupe TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_albums_neuf (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 annee BIGINT,
 genre TEXT,
 source TEXT,
 nb_pistes BIGINT,
 mbid_groupe TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_artistes (
 id BIGINT PRIMARY KEY,
 nom TEXT,
 bio TEXT,
 mbid TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_artistes_neuf (
 id BIGINT PRIMARY KEY,
 nom TEXT,
 bio TEXT,
 mbid TEXT
)
;

INSERT INTO schema_version (version, name) VALUES (110, 'sync_releve')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
