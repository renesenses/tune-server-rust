-- 067_albums_compilation_manuelle.sql
--
-- La décision MANUELLE sur le drapeau « compilation » (#4427).
--
-- `is_compilation` porte ce que le SCAN a déduit. Cette colonne porte ce que
-- l'UTILISATEUR a voulu, et les deux sont des informations différentes :
-- écraser l'une par l'autre rendrait le retour en arrière impossible, et le
-- prochain scan défairait le geste.
--
-- NULL = personne n'a tranché (l'état de toutes les lignes existantes) : le
-- scan décide comme avant. 0/1 = choix explicite, que ni `mark_compilation`
-- ni la passe de réparation ne touchent — même règle que l'enrichissement,
-- où ce qui a été posé à la main est protégé.
--
-- Bertrand, 18/09/2026 : « Coco María Presents » éclaté en douze vignettes,
-- une par artiste de piste. Il veut poser le drapeau à la main, et que ce
-- choix tienne.
--
-- SMALLINT et non BOOLEAN : la convention des booléens de ce schéma, avec la
-- nuance que NULL y est porteur de sens. Idempotent.

BEGIN;

ALTER TABLE albums ADD COLUMN IF NOT EXISTS compilation_manuelle SMALLINT;

-- Le bloc qui suit vise les bases nées de `tune db migrate-to-postgres` : ce
-- chemin crée TOUTES les colonnes en TEXT (`PG_FULL_SCHEMA`, pg_migrate.rs, à
-- dessein — la copie lie chaque valeur SQLite en texte), et compte sur les
-- migrations pour restaurer les vrais types. Sans lui, la colonne resterait
-- TEXT sur ces bases, et `UPDATE albums SET compilation_manuelle = 1` y serait
-- REFUSÉ par PostgreSQL (« column is of type text but expression is of type
-- integer ») : la pose à la main échouerait précisément là. Même forme que 028,
-- à la nuance près que NULL est ici porteur de sens et doit le rester.

DO $migration$
DECLARE
  cur_type TEXT;
BEGIN
  SELECT data_type INTO cur_type
    FROM information_schema.columns
   WHERE table_name = 'albums' AND column_name = 'compilation_manuelle';

  IF cur_type IN ('text', 'character varying') THEN
    ALTER TABLE albums
      ALTER COLUMN compilation_manuelle TYPE SMALLINT
      USING (CASE WHEN compilation_manuelle IS NULL OR compilation_manuelle = '' THEN NULL
                  WHEN compilation_manuelle ~ '^-?[0-9]+$'
                  THEN LEAST(GREATEST(compilation_manuelle::integer, 0), 1)
                  ELSE NULL END)::smallint;
  END IF;
END
$migration$;

INSERT INTO schema_version (version, name) VALUES (67, 'albums_compilation_manuelle')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
