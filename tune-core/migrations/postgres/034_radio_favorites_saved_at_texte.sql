-- radio_favorites.saved_at : du BIGINT au TEXTE, pour rejoindre SQLite.
--
-- Deux fichiers de ce depot declaraient cette colonne differemment :
--   * 005_additional_tables.sql : saved_at BIGINT
--   * db/pg_migrate.rs          : saved_at TEXT
-- Les deux en CREATE TABLE IF NOT EXISTS — celui qui passe le premier gagne,
-- et lequel gagne depend de l'ordre d'installation. C'est exactement le piege
-- que le garde-fou de `network_mounts` decrit dans migrations.rs.
--
-- Le TEXTE est la bonne forme : c'est celle de SQLite, celle que la route
-- ecrit, et celle que le client attend (`saved_at: string`, passe a
-- `new Date(iso)`).
--
-- La conversion est SANS PERTE : l'insertion des favoris radio n'a jamais
-- ecrit cette colonne, et BIGINT n'avait pas de valeur par defaut. Toute ligne
-- existante porte donc NULL. Le `USING` couvre malgre tout le cas ou une
-- installation aurait des epoques en base — on les rend en ISO UTC plutot que
-- de les jeter.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'radio_favorites'
           AND column_name = 'saved_at'
           AND data_type IN ('bigint', 'integer')
    ) THEN
        ALTER TABLE radio_favorites
            ALTER COLUMN saved_at TYPE TEXT
            USING CASE
                WHEN saved_at IS NULL THEN NULL
                ELSE to_char(to_timestamp(saved_at) AT TIME ZONE 'UTC',
                             'YYYY-MM-DD"T"HH24:MI:SS"Z"')
            END;
    END IF;
END $$;

-- Et la meme reparation syntaxique que cote SQLite, pour les lignes ecrites
-- en UTC sans marqueur de fuseau (#1515).
UPDATE radio_favorites
   SET saved_at = REPLACE(saved_at, ' ', 'T') || 'Z'
 WHERE saved_at IS NOT NULL
   AND saved_at <> ''
   AND saved_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}$';
