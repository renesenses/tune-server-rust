-- 026_queue_items_numbering.sql
--
-- #1706 — PostgreSQL : les files d'attente ne sont pas restaurées au démarrage.
--
-- Deux dérives de schéma, constatées ensemble sur la production .15 :
--
--   1. `queue_items` sans `track_number` / `disc_number`. Ces colonnes portent
--      la numérotation par album des titres en streaming (migration SQLite v64).
--      Elles ont été ajoutées au schéma SQLite et à PG_FULL_SCHEMA (le schéma de
--      la migration SQLite→PG, joué UNE seule fois), mais jamais au CREATE TABLE
--      de rattrapage de `ensure_schema`. Toute base dont `queue_items` vient de
--      ce CREATE — c'est le cas de .15 — n'a donc jamais eu ces colonnes, alors
--      que `insert_streaming`, `select_streaming` et `unified_select_base` les
--      nomment toutes les trois : chaque écriture de file échouait avec
--      `column "track_number" of relation "queue_items" does not exist`, et
--      AUCUNE file n'était restaurée au redémarrage (9 zones sur .15).
--
--   2. `streaming_favorites.id` en BIGINT mais avec un DEFAULT de type texte.
--      La migration 012 reconvertit les `id` TEXT des bases migrées en BIGINT +
--      séquence ; `ensure_schema` réimposait ensuite, sans condition, un
--      `SET DEFAULT nextval(...)::text` → `column "id" is of type bigint but
--      default expression is of type text`. Comme le lot entier partait en une
--      seule requête multi-instructions (donc une seule transaction implicite),
--      cet échec annulait tout le reste du lot à chaque démarrage.
--
-- Le garde-fou côté code (une instruction = un aller-retour, DEFAULT texte
-- conditionné au type de la colonne) vit dans `db/postgres.rs`. Cette migration
-- répare l'état déjà installé en base.
--
-- Idempotence :
--   * les colonnes ne sont ajoutées que si elles manquent ;
--   * une colonne déjà présente mais dérivée en TEXT (bases migrées depuis
--     SQLite, où PG_FULL_SCHEMA les déclare TEXT) est convertie en BIGINT, et
--     seulement si toutes ses valeurs sont des entiers — sinon on la laisse
--     telle quelle plutôt que d'interrompre la migration (même prudence
--     que 012/013) ;
--   * le DEFAULT de `streaming_favorites.id` n'est réécrit que s'il est absent
--     ou encore en `::text` alors que la colonne n'est plus du texte ;
--   * `to_regclass` protège les installations neuves où les tables n'existent
--     pas encore (elles seront créées avec les bonnes colonnes).
--
-- Chaque réparation est enveloppée dans son propre bloc EXCEPTION : sur .15 la
-- propriété des tables a dérivé (certaines appartiennent à un rôle différent de
-- celui qui joue les migrations — cf. le commentaire d'appartenance de 012), et
-- un ALTER refusé ferait échouer `run_pg_migrations`, donc le DÉMARRAGE du
-- serveur (state.rs propage l'erreur). Un rattrapage impossible doit se
-- signaler, jamais empêcher le serveur de démarrer.

BEGIN;

DO $migration$
DECLARE
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  col      TEXT;
  cur_type TEXT;
  bad      BIGINT;
BEGIN
  IF to_regclass('queue_items') IS NULL THEN
    RAISE NOTICE 'migration 026: queue_items absente, rien a reparer';
  ELSE
    FOREACH col IN ARRAY ARRAY['track_number', 'disc_number'] LOOP
      SELECT data_type INTO cur_type
        FROM information_schema.columns
       WHERE table_name = 'queue_items' AND column_name = col;

      BEGIN
        IF cur_type IS NULL THEN
          EXECUTE format('ALTER TABLE queue_items ADD COLUMN %I BIGINT', col);
          RAISE NOTICE 'migration 026: queue_items.% ajoutee (bigint)', col;
        ELSIF cur_type IN ('text', 'character varying') THEN
          EXECUTE format(
            'SELECT count(*) FROM queue_items WHERE %I IS NOT NULL AND %I !~ %L',
            col, col, int_re) INTO bad;
          IF bad = 0 THEN
            EXECUTE format('ALTER TABLE queue_items ALTER COLUMN %I DROP DEFAULT', col);
            EXECUTE format(
              'ALTER TABLE queue_items ALTER COLUMN %I TYPE bigint USING NULLIF(%I, '''')::bigint',
              col, col);
            RAISE NOTICE 'migration 026: queue_items.% text->bigint', col;
          ELSE
            RAISE NOTICE 'migration 026: SKIP queue_items.% (% valeurs non entieres)', col, bad;
          END IF;
        END IF;
      EXCEPTION WHEN OTHERS THEN
        -- Droits insuffisants / table verrouillée : on signale et on continue.
        RAISE WARNING 'migration 026: queue_items.% non reparee (%)', col, SQLERRM;
      END;
    END LOOP;
  END IF;
END
$migration$;

DO $migration$
DECLARE
  cur_type TEXT;
  col_def  TEXT;
  maxid    BIGINT;
BEGIN
  IF to_regclass('streaming_favorites') IS NULL THEN
    RAISE NOTICE 'migration 026: streaming_favorites absente, rien a reparer';
  ELSE
    SELECT data_type, column_default INTO cur_type, col_def
      FROM information_schema.columns
     WHERE table_name = 'streaming_favorites' AND column_name = 'id';

    -- Colonne non textuelle (BIGINT depuis 012) dont le DEFAULT manque ou est
    -- resté en `::text` : on lui rend une séquence entière, calée après MAX(id).
    IF cur_type IS NOT NULL
       AND cur_type NOT IN ('text', 'character varying')
       AND (col_def IS NULL OR col_def LIKE '%::text%')
    THEN
      BEGIN
        CREATE SEQUENCE IF NOT EXISTS streaming_favorites_id_seq;
        SELECT COALESCE(MAX(id), 0) INTO maxid FROM streaming_favorites;
        -- setval(seq, MAX) → prochain nextval = MAX+1 ; table vide → 1.
        PERFORM setval('streaming_favorites_id_seq', GREATEST(maxid, 1), maxid > 0);
        ALTER TABLE streaming_favorites
          ALTER COLUMN id SET DEFAULT nextval('streaming_favorites_id_seq');
        RAISE NOTICE 'migration 026: streaming_favorites.id DEFAULT reparee (nextval, prochain %)', maxid + 1;
      EXCEPTION WHEN OTHERS THEN
        RAISE WARNING 'migration 026: streaming_favorites.id DEFAULT non reparee (%)', SQLERRM;
      END;
    END IF;
  END IF;
END
$migration$;

INSERT INTO schema_version (version, name) VALUES (26, 'queue_items_numbering')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
