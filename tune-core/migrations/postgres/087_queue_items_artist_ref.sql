-- 087_queue_items_artist_ref.sql
--
-- #6079 (FabienM, Tune Remote Android) : l'ARTISTE d'une piste de service chez
-- son service (`StreamTrack.artist_id`), gardé avec la ligne de file. Jumelle
-- de la migration SQLite 123.
--
-- `GET /zones/{id}/queue` le rend sous `artist_id_service`, qui valait `null`
-- en dur : « Aller à l'artiste » depuis la file cherchait l'artiste par son
-- nom, et un homonyme menait à une autre fiche.
--
-- NULL pour les lignes existantes. TEXT comme côté SQLite : rien à rattraper
-- dans la parité de types.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

-- `queue_items` n'est pas garantie ici selon le chemin (elle peut naître de
-- `PG_FULL_SCHEMA` ou d'`ENSURE_TABLES`) : même garde `to_regclass` que la
-- 078. La colonne est alors posée par `ENSURE_COLUMNS` (postgres.rs) et par
-- `PG_FULL_SCHEMA` (pg_migrate.rs).
DO $artiste_de_service$
BEGIN
    IF to_regclass('queue_items') IS NOT NULL THEN
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS artist_ref TEXT;
    ELSE
        RAISE NOTICE 'migration 087 : queue_items absente';
    END IF;
END $artiste_de_service$;

INSERT INTO schema_version (version, name) VALUES (87, 'queue_items_artist_ref')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
