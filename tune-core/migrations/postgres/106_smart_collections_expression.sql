-- 106_smart_collections_expression.sql
--
-- #7872 : la requête avancée d'une collection intelligente, par exemple
-- `composer:beethoven conductor:karajan year:1960..1969 -genre:live`. Elle est
-- évaluée à chaque ouverture par le compilateur de `library::requete`, avec
-- des paramètres liés. NULL : la collection n'a que ses règles.
-- Jumelle de la migration SQLite 140.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE smart_collections
    ADD COLUMN IF NOT EXISTS expression TEXT;

INSERT INTO schema_version (version, name) VALUES (106, 'smart_collections_expression')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
