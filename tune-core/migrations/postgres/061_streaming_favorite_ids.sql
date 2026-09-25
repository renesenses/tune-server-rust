-- #3715: align native streaming favorite IDs with SQLite and migrated PG.
-- Convert only the ID, preserving rows and constraints. Invalid/out-of-range
-- values or colliding numeric IDs abort the transaction before touching the
-- sequence. Already-consumed sequence values must never be reused.
BEGIN;
DO $migration$
DECLARE
  typ TEXT;
  max_id BIGINT;
  sequence_value BIGINT;
BEGIN
  SELECT data_type INTO typ FROM information_schema.columns
   WHERE table_schema = current_schema() AND table_name = 'streaming_favorites'
     AND column_name = 'id';
  IF typ IS NULL THEN
    RETURN; -- ensure_schema creates a BIGINT table if absent on this path.
  END IF;
  IF typ NOT IN ('text', 'character varying', 'smallint', 'integer', 'bigint') THEN
    RAISE EXCEPTION 'migration 061: unsupported streaming_favorites.id type %', typ;
  END IF;
  -- Also serialize inserts while repairing an already-BIGINT table's sequence.
  LOCK TABLE streaming_favorites IN ACCESS EXCLUSIVE MODE;
  IF typ <> 'bigint' THEN
    ALTER TABLE streaming_favorites ALTER COLUMN id DROP DEFAULT;
    ALTER TABLE streaming_favorites ALTER COLUMN id TYPE BIGINT USING id::bigint;
  END IF;
  CREATE SEQUENCE IF NOT EXISTS streaming_favorites_id_seq;
  ALTER TABLE streaming_favorites ALTER COLUMN id
    SET DEFAULT nextval('streaming_favorites_id_seq');
  SELECT max(id) INTO max_id FROM streaming_favorites;
  SELECT last_value INTO sequence_value FROM streaming_favorites_id_seq;
  -- Leave an ahead-of-data sequence AND its is_called flag untouched.
  -- At equality the next value must skip the existing row, even if uncalled.
  IF max_id >= sequence_value THEN
    PERFORM setval('streaming_favorites_id_seq', max_id, true);
  END IF;
END
$migration$;
INSERT INTO schema_version (version, name) VALUES (61, 'streaming_favorite_ids')
ON CONFLICT (version) DO NOTHING;
COMMIT;
