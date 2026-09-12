-- 047_listen_history_album_id_bigint.sql
--
-- `listen_history.album_id` est TEXT sur TOUTE installation PostgreSQL, alors
-- que `albums.id` est BIGINT. La jointure de « Continuer l'ecoute »
-- (`HISTORIQUE_VERS_ALBUM`, tune-server/src/routes/home.rs) compare les deux :
--
--   ERROR:  operator does not exist: text = bigint
--   LIGNE 3 : JOIN albums a ON (lh.album_id = a.id ...
--
-- L'erreur est avalee par le `unwrap_or_default()` de l'appelant : la section
-- n'affiche pas un message, elle disparait. Meme cause pour les albums resolus
-- de `resoudre_albums` et pour `sql_top_genres` (#2860).
--
-- # Pourquoi la colonne est restee TEXT
--
-- La 012 la convertit deja — `['listen_history','album_id']` figure dans son
-- `fk_cols`. Elle ne l'a jamais vue : `album_id` n'est ajoutee par AUCUN script
-- numerote. Elle arrive par `ENSURE_COLUMNS` (tune-core/src/db/postgres.rs),
-- que `ensure_schema()` rejoue a chaque demarrage — et `PostgresDb::connect()`
-- appelle `ensure_schema()` AVANT `run_pg_migrations()` seulement au boot
-- suivant. Au moment ou la 012 s'est executee, la colonne n'existait pas :
-- son garde « ne toucher que ce qui est encore text » en a fait un no-op, et
-- la 012 s'est inscrite dans `schema_version` pour toujours.
--
-- La colonne apparait donc APRES, en TEXT, et plus aucune migration ne repasse.
--
-- # Ce que fait ce script
--
-- Rejoue la conversion de la 012 sur cette seule colonne. A ce stade la
-- colonne EXISTE (ENSURE_COLUMNS l'a posee a un demarrage precedent), donc le
-- garde la voit.
--
-- `ENSURE_COLUMNS` declare desormais `BIGINT` : une base neuve ne repasse
-- jamais par ici, et une base existante est reparee une fois.
--
-- Surete — reprise mot pour mot des gardes de la 012 :
--   * idempotent : on ne touche la colonne que tant qu'elle est text/varchar,
--     donc no-op sur une base deja convertie ou deja neuve ;
--   * cast garde : conversion UNIQUEMENT si toute valeur non nulle est un
--     entier litteral. Sinon on saute avec un NOTICE plutot que d'avorter ;
--   * aucun declencheur ne porte sur `listen_history` (les `*_search_tsv_trg`
--     de la 002 sont sur artists/albums/tracks), donc pas de DROP/CREATE
--     TRIGGER a orchestrer comme le fait la 012 ;
--   * la colonne n'a pas de DEFAULT, rien a retablir.

BEGIN;

DO $migration$
DECLARE
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  cur_type TEXT;
  bad      BIGINT;
BEGIN
  SELECT data_type INTO cur_type
    FROM information_schema.columns
   WHERE table_name = 'listen_history' AND column_name = 'album_id';

  IF cur_type IN ('text', 'character varying') THEN
    SELECT count(*) INTO bad
      FROM listen_history
     WHERE album_id IS NOT NULL AND album_id !~ int_re;

    IF bad = 0 THEN
      ALTER TABLE listen_history
        ALTER COLUMN album_id TYPE bigint USING album_id::bigint;
      RAISE NOTICE 'migration 047: listen_history.album_id text->bigint';
    ELSE
      RAISE NOTICE 'migration 047: SKIP listen_history.album_id (% valeurs non entieres)', bad;
    END IF;
  END IF;
END
$migration$;

INSERT INTO schema_version (version, name)
VALUES (47, 'listen_history_album_id_bigint')
ON CONFLICT (version) DO NOTHING;

COMMIT;
