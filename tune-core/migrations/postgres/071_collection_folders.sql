-- collection_folders / collection_folder_items : ranger les collections (des
-- DEUX sortes) dans un arbre de dossiers — #4853, Gros Bidon fil 1907.
-- Décision de Bertrand du 24/09/2026 : arbre, profondeur maximale 3.
--
-- Jumelle PostgreSQL de la migration SQLite 108. Voir celle-ci pour la
-- doctrine complète ; l'essentiel :
--
-- * une ligne de rangement porte TOUJOURS la paire `(kind, collection_id)` :
--   les collections simples (réglage JSON `collections`) et intelligentes
--   (`smart_collections`) ont des identifiants qui SE RECOUVRENT ;
-- * la clef primaire `(kind, collection_id)` garantit qu'une collection est
--   dans UN SEUL dossier ;
-- * `NULL` = racine ; aucune clef étrangère, cycle et profondeur vérifiés
--   par le dépôt ;
-- * `collection_id` en BIGINT : `smart_collections.id` est BIGINT depuis la
--   012, et l'id d'une collection simple est un i64 (max + 1).
-- * PAS de BIGSERIAL : l'identifiant de dossier est attribué par le dépôt,
--   pour éviter la divergence de séquence de la bascule (#1706).
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS, conversions gardées.

BEGIN;

CREATE TABLE IF NOT EXISTS collection_folders (
    id BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    parent_id BIGINT,
    position BIGINT NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE INDEX IF NOT EXISTS idx_collection_folders_parent ON collection_folders(parent_id);

CREATE TABLE IF NOT EXISTS collection_folder_items (
    kind TEXT NOT NULL,
    collection_id BIGINT NOT NULL,
    folder_id BIGINT,
    position BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, collection_id)
);
CREATE INDEX IF NOT EXISTS idx_collection_folder_items_folder ON collection_folder_items(folder_id);

-- Rattrapage de la bascule SQLite -> PostgreSQL : `PG_FULL_SCHEMA` crée ces
-- deux tables en TOUT TEXTE (la copie lie chaque valeur en texte), et le
-- `CREATE TABLE IF NOT EXISTS` ci-dessus ne les corrige pas. Conversion
-- gardée sur le type courant (patron 038) : strict no-op une fois BIGINT.
DO $$
DECLARE
    t TEXT;
    c TEXT;
    d TEXT;
BEGIN
    FOR t, c, d IN
        SELECT * FROM (VALUES
            ('collection_folders', 'id', NULL),
            ('collection_folders', 'parent_id', NULL),
            ('collection_folders', 'position', '0'),
            ('collection_folder_items', 'collection_id', NULL),
            ('collection_folder_items', 'folder_id', NULL),
            ('collection_folder_items', 'position', '0')
        ) AS v(t, c, d)
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.columns
             WHERE table_name = t
               AND column_name = c
               AND data_type IN ('text', 'character varying')
        ) THEN
            EXECUTE format('ALTER TABLE %I ALTER COLUMN %I DROP DEFAULT', t, c);
            EXECUTE format(
                'ALTER TABLE %I ALTER COLUMN %I TYPE BIGINT USING NULLIF(TRIM(%I), '''')::BIGINT',
                t, c, c
            );
            IF d IS NOT NULL THEN
                EXECUTE format('ALTER TABLE %I ALTER COLUMN %I SET DEFAULT %s', t, c, d);
            END IF;
        END IF;
    END LOOP;
END $$;

INSERT INTO schema_version (version, name) VALUES (71, 'collection_folders') ON CONFLICT (version) DO NOTHING;

COMMIT;
