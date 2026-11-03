-- 094_tracks_bitrate_kbps.sql
--
-- #6332 (fil forum 2269) : le DEBIT MOYEN du fichier en kbit/s, lu au scan
-- (lofty `audio_bitrate`) et affiche dans « Lecture en cours ».
-- Jumelle de la migration SQLite 130.
--
-- BIGINT, comme les autres entiers de `tracks` cote PostgreSQL ; INTEGER cote
-- SQLite, meme famille. NULL pour les lignes existantes : le rescan ordinaire
-- complete les fichiers inchanges sans relecture complete.
--
-- Deux naissances possibles (voir `pg_sqlite_type_parity.rs`) :
-- * installation native : la colonne n'existe pas, elle est posee BIGINT ;
-- * bascule depuis SQLite : `PG_FULL_SCHEMA` l'a creee TEXT pour la copie ;
--   elle est CONVERTIE ici. Une valeur qui n'est pas un entier devient NULL
--   (le rescan la recalcule), plutot que de bloquer la conversion.
--
-- Idempotent : sans effet une fois la colonne en BIGINT.

BEGIN;

ALTER TABLE tracks ADD COLUMN IF NOT EXISTS bitrate_kbps BIGINT;

DO $debit_de_piste$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'tracks'
       AND column_name = 'bitrate_kbps';
    IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
        ALTER TABLE tracks ALTER COLUMN bitrate_kbps TYPE BIGINT USING (
            CASE WHEN bitrate_kbps::text ~ '^[0-9]+$' THEN bitrate_kbps::text::bigint END
        );
        RAISE NOTICE 'migration 094 : tracks.bitrate_kbps % -> bigint', typ;
    END IF;
END $debit_de_piste$;

INSERT INTO schema_version (version, name) VALUES (94, 'tracks_bitrate_kbps')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
