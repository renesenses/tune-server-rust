-- 079_listen_history_album_id_index.sql
--
-- Fil 2130 (04/10/2026) : « Reprendre l'écoute » tombait en « (delai) » sur
-- PostgreSQL. La jointure de l'historique vers l'album (`OR` entre la clé et
-- un repli par titre et artiste) prenait 25,7 s pour 8 571 albums, quand le
-- widget abandonne à 8 s. Elle est réécrite en deux branches `UNION ALL`
-- (`home_queries::historique_rattache_a_son_album`) ; cet index sert la
-- première (`a.id = lh.album_id`) et isole la seconde (`album_id IS NULL`).
-- Jumelle de la migration SQLite 115.
--
-- Numérotée 079 : la 078 est celle de #5706 (Bandcamp, fil 2121), qui doit
-- être fusionnée AVANT celle-ci.
--
-- GARDE : `listen_history.album_id` n'est créée par AUCUN script numéroté.
-- Elle arrive par `ENSURE_COLUMNS` (tune-core/src/db/postgres.rs) ou par
-- `PG_FULL_SCHEMA` (bascule). Sur une base neuve, les scripts passent avant
-- que la colonne n'existe : l'index est alors sauté ici, sans erreur, et
-- `ENSURE_COLUMNS` le pose juste après la colonne, au second passage
-- d'`ensure_schema()`. `to_regclass` rend NULL si la table manque.
--
-- Idempotent : CREATE INDEX IF NOT EXISTS est sûr à rejouer.

BEGIN;

DO $migration$
BEGIN
  IF EXISTS (
    SELECT 1 FROM pg_attribute
     WHERE attrelid = to_regclass('listen_history')
       AND attname = 'album_id'
       AND NOT attisdropped
  ) THEN
    CREATE INDEX IF NOT EXISTS idx_listen_history_album_id
      ON listen_history(album_id);
  ELSE
    RAISE NOTICE 'migration 079: listen_history.album_id absente, index laissé à ENSURE_COLUMNS';
  END IF;
END
$migration$;

INSERT INTO schema_version (version, name) VALUES (79, 'listen_history_album_id_index')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
