-- 049_profile_id_bigint.sql
--
-- `listen_history.profile_id` et `playlists.profile_id` sont TEXT sur toute
-- installation PostgreSQL NATIVE, alors que `profiles.id` est BIGINT et que le
-- code lie ces colonnes en `i64`.
--
--   PlaylistRepo::list()  : SELECT ... FROM playlists p WHERE p.profile_id = $1
--   PlaylistRepo::count() : SELECT COUNT(*) FROM playlists WHERE profile_id = $1
--   history_repo.rs       : ... WHERE profile_id = <entier interpole>
--
-- Mesure du 31/08/2026, sur une base montee exactement comme le fait
-- `connect()` (scripts numerotes puis `ensure_schema`) :
--
--   ERROR:  operator does not exist: text = bigint
--   LINE 1: ... AS SELECT p.id FROM playlists p WHERE p.profile_id = $1;
--
--   ERROR:  operator does not exist: text = bigint
--   LINE 1: ... AS SELECT id FROM listen_history WHERE profile_id = $1;
--
-- Consequence : sur un serveur PostgreSQL natif, la liste des playlists est
-- vide et l'historique filtre par profil ne rend rien. Le meme silence que
-- #2860 — l'echec SQL remonte a un appelant qui le traduit en liste vide.
--
-- # Pourquoi elles sont restees TEXT
--
-- Exactement la cause de #2860, et c'est le sujet de #2995. La 012 les vise
-- toutes les deux : `['listen_history','profile_id']` et
-- `['playlists','profile_id']` figurent dans son `fk_cols`. Elle ne les a
-- jamais vues.
--
-- Ces deux colonnes n'arrivent par AUCUN script numerote — seulement par
-- `ENSURE_COLUMNS` (tune-core/src/db/postgres.rs). Or au TOUT PREMIER
-- demarrage d'une base PostgreSQL native, `PostgresDb::connect()` appelle
-- `ensure_schema()` avant `run_pg_migrations()` : les tables `playlists` et
-- `listen_history` n'existent pas encore, les deux `ALTER TABLE` echouent
-- (journalises en `warn!`, puis avales), et c'est `run_pg_migrations()` qui
-- cree les tables dans la foulee — 012 comprise, qui s'inscrit dans
-- `schema_version` pour toujours.
--
-- Les colonnes apparaissent au demarrage SUIVANT, en TEXT, et plus aucune
-- migration ne repasse.
--
-- Verifie par la mesure : sur les 104 colonnes visees par les migrations de
-- rattrapage 010, 011, 012 et 013, CINQ sont absentes de la base au moment ou
-- leur migration s'execute. Trois s'en sortent — `streaming_favorites.id` et
-- `streaming_favorites.profile_id` parce que `ENSURE_TABLES` cree la table par
-- un `CREATE TABLE IF NOT EXISTS` qui, lui, reussit des le premier demarrage ;
-- `listen_history.album_id` parce que la 047 l'a reparee (#2860). Restent ces
-- deux-ci. C'est la reponse complete a la question de #2995 : le defaut
-- d'ordonnancement touche 5 colonnes, pas 40, et il en reste 2 a reparer.
--
-- # Ce que fait ce script
--
-- Rejoue la conversion de la 012 sur ces deux colonnes. A ce stade elles
-- EXISTENT (`ENSURE_COLUMNS` les a posees a un demarrage precedent), donc le
-- garde les voit.
--
-- `ENSURE_COLUMNS` declare desormais `BIGINT` pour les deux : une base neuve
-- ne repassera jamais par ici, une base existante est reparee une fois.
--
-- ⚠️ `PG_FULL_SCHEMA` les garde en TEXT, et c'est VOLONTAIRE : la copie
-- SQLite -> PG lie chaque valeur en parametre texte, un type numerique y ferait
-- echouer l'INSERT de la table entiere. Sur ce chemin-la c'est la 012 qui
-- convertit apres la copie, et elle les voit — mesure : une base migree rend
-- bien `bigint` pour les deux.
--
-- Surete — reprise mot pour mot des gardes de la 012 et de la 047 :
--   * idempotent : on ne touche la colonne que tant qu'elle est text/varchar,
--     donc no-op sur une base deja convertie, deja neuve, ou migree ;
--   * cast garde : conversion UNIQUEMENT si toute valeur non nulle est un
--     entier litteral. Sinon on saute avec un NOTICE plutot que d'avorter ;
--   * le DEFAULT texte de `playlists.profile_id` (`'1'`) ne peut pas etre cast
--     pendant l'ALTER TYPE : on le retire d'abord et on le repose en entier,
--     comme le fait la 012. Sans quoi la colonne NOT NULL deviendrait
--     ininserrable ;
--   * aucun declencheur ne porte sur `playlists` ni `listen_history` (les
--     `*_search_tsv_trg` de la 002 sont sur artists/albums/tracks), donc pas de
--     DROP/CREATE TRIGGER a orchestrer comme le fait la 012.

BEGIN;

DO $migration$
DECLARE
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  cols     TEXT[][] := ARRAY[
    ['listen_history','profile_id'],
    ['playlists','profile_id']
  ];
  c        TEXT[];
  cur_type TEXT;
  col_def  TEXT;
  bad      BIGINT;
BEGIN
  FOREACH c SLICE 1 IN ARRAY cols LOOP
    SELECT data_type, column_default INTO cur_type, col_def
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];

    IF cur_type IN ('text', 'character varying') THEN
      EXECUTE format(
        'SELECT count(*) FROM %I WHERE %I IS NOT NULL AND %I !~ %L',
        c[1], c[2], c[2], int_re) INTO bad;

      IF bad = 0 THEN
        IF col_def IS NOT NULL THEN
          EXECUTE format('ALTER TABLE %I ALTER COLUMN %I DROP DEFAULT', c[1], c[2]);
        END IF;
        EXECUTE format(
          'ALTER TABLE %I ALTER COLUMN %I TYPE bigint USING %I::bigint',
          c[1], c[2], c[2]);
        IF col_def ~ '^''?-?[0-9]+''?(::text)?$' THEN
          EXECUTE format('ALTER TABLE %I ALTER COLUMN %I SET DEFAULT %s',
            c[1], c[2], regexp_replace(col_def, '[^0-9-]', '', 'g'));
        END IF;
        RAISE NOTICE 'migration 049: %.% text->bigint', c[1], c[2];
      ELSE
        RAISE NOTICE 'migration 049: SKIP %.% (% valeurs non entieres)', c[1], c[2], bad;
      END IF;
    END IF;
  END LOOP;
END
$migration$;

INSERT INTO schema_version (version, name)
VALUES (49, 'profile_id_bigint')
ON CONFLICT (version) DO NOTHING;

COMMIT;
