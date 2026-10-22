-- 092_queue_items_contexte.sql
--
-- #6466 (FabienM, fil 2302 points 10, 11, 15) : le CONTENEUR de chaque élément
-- de file — l'album, la playlist ou l'artiste dont il a été enfilé, ou
-- `track` pour un titre ajouté seul. L'historique l'écrit à l'écoute, au lieu
-- du seul contexte de session de la zone. Jumelle de la migration SQLite 128.
--
-- NULL pour les lignes existantes (= contexte de session, comme avant). TEXT
-- comme côté SQLite : rien à rattraper dans la parité de types.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

-- Même garde `to_regclass` que la 087 : `queue_items` peut naître de
-- `PG_FULL_SCHEMA` ou d'`ENSURE_TABLES`, qui posent alors les colonnes.
DO $contexte_de_file$
BEGIN
    IF to_regclass('queue_items') IS NOT NULL THEN
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS context_type TEXT;
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS context_id TEXT;
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS context_source TEXT;
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS context_title TEXT;
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS context_cover TEXT;
    ELSE
        RAISE NOTICE 'migration 092 : queue_items absente';
    END IF;
END $contexte_de_file$;

INSERT INTO schema_version (version, name) VALUES (92, 'queue_items_contexte')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
