-- 091_tags_position.sql
--
-- #6257 (web#2166, suite de web#1715) : l'ORDRE des etiquettes, choisi par
-- l'utilisateur sur la page Etiquettes. Jumelle de la migration SQLite 127.
--
-- BIGINT NOT NULL DEFAULT 0, comme les autres entiers cote PostgreSQL ;
-- INTEGER cote SQLite, meme famille.
--
-- Deux naissances possibles (voir `pg_sqlite_type_parity.rs`) :
-- * installation native : la colonne n'existe pas, elle est posee BIGINT ;
-- * bascule depuis SQLite : `PG_FULL_SCHEMA` l'a creee TEXT pour la copie ;
--   elle est CONVERTIE ici. Une valeur qui n'est pas un entier devient 0
--   (le nom departage), plutot que de bloquer la conversion.
--
-- Les etiquettes existantes restent a 0, « jamais placees » : la liste reste
-- ALPHABETIQUE tant que l'utilisateur n'a rien reordonne (decision de
-- Bertrand du 13/10). Seul `PUT /tags/order` ecrit des positions (1 a n) ;
-- un ordre deja choisi (copie par une bascule) n'est jamais touche.
--
-- Idempotent : sans effet une fois la colonne en BIGINT et l'ordre pose.

BEGIN;

ALTER TABLE tags ADD COLUMN IF NOT EXISTS position BIGINT NOT NULL DEFAULT 0;

DO $ordre_des_etiquettes$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'tags'
       AND column_name = 'position';
    IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
        ALTER TABLE tags ALTER COLUMN position DROP DEFAULT;
        ALTER TABLE tags ALTER COLUMN position TYPE BIGINT USING (
            CASE WHEN position::text ~ '^-?[0-9]+$' THEN position::text::bigint ELSE 0 END
        );
        RAISE NOTICE 'migration 091 : tags.position % -> bigint', typ;
    END IF;
END $ordre_des_etiquettes$;

UPDATE tags SET position = 0 WHERE position IS NULL;
ALTER TABLE tags ALTER COLUMN position SET DEFAULT 0;
ALTER TABLE tags ALTER COLUMN position SET NOT NULL;

INSERT INTO schema_version (version, name) VALUES (91, 'tags_position')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
