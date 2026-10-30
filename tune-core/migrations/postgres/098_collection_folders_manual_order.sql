-- 098_collection_folders_manual_order.sql
--
-- #6779 (Didier, fil 1907) : l'ORDRE des rayons, alphabétique par défaut,
-- manuel en option (décision de Bertrand du 19/10/2026). Jumelle de la
-- migration SQLite 134.
--
-- `manual_order` = 1 : la fratrie de ce dossier est rangée à la main, par
-- `position` ; 0 : elle suit l'ordre naturel des noms. Les dossiers
-- existants restent à 0 — leur position n'était que leur ordre de création.
--
-- BIGINT NOT NULL DEFAULT 0, comme les autres entiers côté PostgreSQL ;
-- INTEGER côté SQLite, même famille.
--
-- Deux naissances possibles (voir `pg_sqlite_type_parity.rs`) :
-- * installation native : la colonne n'existe pas, elle est posée BIGINT ;
-- * bascule depuis SQLite : `PG_FULL_SCHEMA` l'a créée TEXT pour la copie ;
--   elle est CONVERTIE ici, valeurs gardées. Une valeur qui n'est pas un
--   entier devient 0 (alphabétique), plutôt que de bloquer la conversion.
--
-- Idempotent : sans effet une fois la colonne en BIGINT. Rejouée sur une base
-- de bascule (sentinelle 99), elle ne touche pas un ordre recopié.

BEGIN;

ALTER TABLE collection_folders ADD COLUMN IF NOT EXISTS manual_order BIGINT NOT NULL DEFAULT 0;

DO $ordre_des_rayons$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'collection_folders'
       AND column_name = 'manual_order';
    IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
        ALTER TABLE collection_folders ALTER COLUMN manual_order DROP DEFAULT;
        ALTER TABLE collection_folders ALTER COLUMN manual_order TYPE BIGINT USING (
            CASE WHEN manual_order::text ~ '^-?[0-9]+$' THEN manual_order::text::bigint ELSE 0 END
        );
        RAISE NOTICE 'migration 098 : collection_folders.manual_order % -> bigint', typ;
    END IF;
END $ordre_des_rayons$;

UPDATE collection_folders SET manual_order = 0 WHERE manual_order IS NULL;
ALTER TABLE collection_folders ALTER COLUMN manual_order SET DEFAULT 0;
ALTER TABLE collection_folders ALTER COLUMN manual_order SET NOT NULL;

INSERT INTO schema_version (version, name) VALUES (98, 'collection_folders_manual_order')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
