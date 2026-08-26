-- #2468 — 005 cree bookmarks.position_ms en INTEGER sur une base PostgreSQL
-- neuve, alors que le code lui transmet des i64 et que le schema canonique de
-- pg_migrate.rs la declare en BIGINT. La migration 013 promettait la
-- convergence, mais ne traitait que TEXT/VARCHAR : un INTEGER etait ignore.
--
-- Nouvelle migration plutot que modification de 013 : les installations qui
-- ont deja enregistre la version 13 ne rejoueraient jamais ce fichier.
-- INTEGER -> BIGINT est une conversion exacte et sans perte. Le garde rend le
-- script idempotent sur les bases deja conformes ou partielles.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'bookmarks'
           AND column_name = 'position_ms'
           AND data_type IN ('smallint', 'integer')
    ) THEN
        ALTER TABLE bookmarks
            ALTER COLUMN position_ms TYPE BIGINT
            USING position_ms::bigint;
    END IF;
END $$;

INSERT INTO schema_version (version, name)
VALUES (36, 'bookmarks_position_bigint')
ON CONFLICT (version) DO NOTHING;
