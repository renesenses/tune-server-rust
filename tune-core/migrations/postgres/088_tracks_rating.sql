-- 088_tracks_rating.sql
--
-- #6123 (forum-hifi #792) : la NOTE de la piste lue dans ses balises
-- (Vorbis RATING, ID3 POPM / TXXX:RATING, MP4 rate, FMPS), ramenee sur 0 a 5.
-- Jumelle de la migration SQLite 124.
--
-- BIGINT, comme les autres entiers de `tracks` cote PostgreSQL ; INTEGER cote
-- SQLite, meme famille. NULL pour les lignes existantes : la note arrive au
-- prochain scan qui recrit la ligne.
--
-- Deux naissances possibles (voir `pg_sqlite_type_parity.rs`) :
-- * installation native : la colonne n'existe pas, elle est posee BIGINT ;
-- * bascule depuis SQLite : `PG_FULL_SCHEMA` l'a creee TEXT pour la copie ;
--   elle est CONVERTIE ici. Une valeur qui n'est pas un entier devient NULL
--   (le scan la recrit), plutot que de bloquer la conversion.
--
-- Idempotent : sans effet une fois la colonne en BIGINT.

BEGIN;

ALTER TABLE tracks ADD COLUMN IF NOT EXISTS rating BIGINT;

DO $note_de_piste$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'tracks'
       AND column_name = 'rating';
    IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
        ALTER TABLE tracks ALTER COLUMN rating TYPE BIGINT USING (
            CASE WHEN rating::text ~ '^[0-9]+$' THEN rating::text::bigint END
        );
        RAISE NOTICE 'migration 088 : tracks.rating % -> bigint', typ;
    END IF;
END $note_de_piste$;

INSERT INTO schema_version (version, name) VALUES (88, 'tracks_rating')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
