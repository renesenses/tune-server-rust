-- 028_albums_is_compilation.sql
--
-- Le drapeau « compilation » de l'album (#1957).
--
-- Il était lu dans les tags (`TCMP`, et son alias `TCP`), utilisé pendant le
-- scan pour décider du regroupement par artiste d'album — puis jeté. La table
-- `albums` n'avait aucune colonne pour lui : aucune requête ne pouvait le
-- rendre, aucun écran l'afficher. Bertrand : « dans la vue album, le tag
-- compilation n'apparaît jamais ».
--
-- Type : SMALLINT 0/1, la convention des booléens de ce schéma (`muted`,
-- `is_hidden`, `*_enabled` — cf. l'en-tête de 001 et le commentaire de 013).
-- DEFAULT 0 : les lignes existantes valent « non », et le prochain scan lève
-- le drapeau sur les disques qu'il regroupe en Various Artists.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.
--
-- Le bloc de réparation qui suit vise les bases nées de
-- `tune db migrate-to-postgres` : ce chemin crée TOUTES les colonnes en TEXT
-- (`PG_FULL_SCHEMA`, pg_migrate.rs, à dessein — la copie lie chaque valeur
-- SQLite en texte), et compte sur les migrations pour restaurer les vrais
-- types. Sans lui, `is_compilation` resterait TEXT sur ces bases, et
-- `ADD COLUMN IF NOT EXISTS` n'y changerait rien. Même forme que 010.

BEGIN;

ALTER TABLE albums ADD COLUMN IF NOT EXISTS is_compilation SMALLINT DEFAULT 0;

DO $migration$
DECLARE
  cur_type TEXT;
BEGIN
  SELECT data_type INTO cur_type
    FROM information_schema.columns
   WHERE table_name = 'albums' AND column_name = 'is_compilation';

  -- On ne touche QUE la colonne encore en texte : no-op sur un schéma déjà
  -- conforme (installation neuve). Une valeur qui n'est pas un entier
  -- (inattendu) devient 0 plutôt que d'interrompre la migration.
  IF cur_type IN ('text', 'character varying') THEN
    ALTER TABLE albums
      ALTER COLUMN is_compilation DROP DEFAULT;
    ALTER TABLE albums
      ALTER COLUMN is_compilation TYPE SMALLINT
      USING (CASE WHEN is_compilation ~ '^-?[0-9]+$'
                  THEN LEAST(GREATEST(is_compilation::integer, 0), 1)
                  ELSE 0 END)::smallint;
    ALTER TABLE albums
      ALTER COLUMN is_compilation SET DEFAULT 0;
  END IF;
END
$migration$;

UPDATE albums SET is_compilation = 0 WHERE is_compilation IS NULL;

INSERT INTO schema_version (version, name) VALUES (28, 'albums_is_compilation')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
