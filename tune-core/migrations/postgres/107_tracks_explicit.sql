-- 107_tracks_explicit.sql
--
-- #8088 (controle parental) : le MARQUAGE EXPLICITE de la balise du fichier
-- (ITUNESADVISORY = 1 ou 4, MP4 rtng), lu au scan. 1 explicite, 0 propre,
-- NULL sans balise ou pas encore relu. Jumelle de la migration SQLite 141.
--
-- BIGINT, comme les autres entiers de `tracks` cote PostgreSQL ; INTEGER cote
-- SQLite, meme famille.
--
-- Deux naissances possibles (voir `pg_sqlite_type_parity.rs`) :
-- * installation native : la colonne n'existe pas, elle est posee BIGINT ;
-- * bascule depuis SQLite : `PG_FULL_SCHEMA` l'a creee TEXT pour la copie ;
--   elle est CONVERTIE ici. Une valeur qui n'est pas un entier devient NULL
--   (le rescan la relit), plutot que de bloquer la conversion.
--
-- Index partiel des pistes marquees, pose APRES la conversion.
--
-- Idempotent : sans effet une fois la colonne en BIGINT.

BEGIN;

ALTER TABLE tracks ADD COLUMN IF NOT EXISTS explicit BIGINT;

DO $piste_explicite$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'tracks'
       AND column_name = 'explicit';
    IF typ IN ('text', 'character varying', 'integer', 'smallint', 'boolean') THEN
        ALTER TABLE tracks ALTER COLUMN explicit TYPE BIGINT USING (
            CASE WHEN explicit::text ~ '^[0-9]+$' THEN explicit::text::bigint END
        );
        RAISE NOTICE 'migration 107 : tracks.explicit % -> bigint', typ;
    END IF;
END $piste_explicite$;

CREATE INDEX IF NOT EXISTS idx_tracks_explicit ON tracks(id) WHERE explicit = 1;

INSERT INTO schema_version (version, name) VALUES (107, 'tracks_explicit')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
