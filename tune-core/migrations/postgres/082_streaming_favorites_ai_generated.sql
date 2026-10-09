-- 082_streaming_favorites_ai_generated.sql
--
-- #5530 (FabienM, fil 2053) : le marquage « généré par IA » de Qobuz, gardé
-- avec le favori de service. Jumelle de la migration SQLite 118.
--
-- Qobuz le porte au niveau de l'ALBUM (`album/get` : `"ai_generated": true`,
-- relevé par la sonde `raw-keys` sur le .18 le 05/10/2026). '1' = marqué,
-- '0' = le service dit non, NULL = inconnu. TEXT comme côté SQLite et comme
-- `album_ref` : rien à rattraper dans la parité de types.
--
-- `streaming_favorites` n'est pas garantie ici (elle naît de `PG_FULL_SCHEMA`
-- ou de `ENSURE_TABLES`) : même garde `to_regclass` que la 078 ; la colonne
-- est alors posée par `ENSURE_COLUMNS` (postgres.rs) et `PG_FULL_SCHEMA`
-- (pg_migrate.rs).
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

DO $marquage_ia$
BEGIN
    IF to_regclass('streaming_favorites') IS NOT NULL THEN
        ALTER TABLE streaming_favorites ADD COLUMN IF NOT EXISTS ai_generated TEXT;
    ELSE
        RAISE NOTICE 'migration 082 : streaming_favorites absente';
    END IF;
END $marquage_ia$;

INSERT INTO schema_version (version, name) VALUES (82, 'streaming_favorites_ai_generated')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
