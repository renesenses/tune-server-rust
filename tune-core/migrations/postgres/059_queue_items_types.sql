-- #3716: use the existing startup BIGINT representation on every path.
-- Widen INTEGER/SMALLINT rather than narrowing values already stored by
-- ensure_schema. The queue writers bind i64 or integer SQL literals, including
-- is_current (0/1); track_number/disc_number already agree as BIGINT.
-- Older SQLite imports can carry TEXT. Validate both syntax and range before
-- converting; malformed legacy values remain intact and produce a NOTICE.
BEGIN;
DO $migration$
DECLARE
  col TEXT;
  typ TEXT;
  def TEXT;
  checked_value BIGINT;
BEGIN
  FOREACH col IN ARRAY ARRAY['position', 'is_current', 'duration_ms'] LOOP
    SELECT data_type, column_default INTO typ, def
      FROM information_schema.columns
     WHERE table_schema = 'public' AND table_name = 'queue_items'
       AND column_name = col;
    IF typ IN ('smallint', 'integer', 'text', 'character varying') THEN
      IF typ IN ('text', 'character varying') THEN
        BEGIN
          -- Evaluate the cast without changing a row. Catch syntax AND range
          -- errors, using the same bigint input rules as the ALTER below.
          EXECUTE format('SELECT max(%I::bigint) FROM public.queue_items', col)
            INTO checked_value;
        EXCEPTION WHEN invalid_text_representation OR numeric_value_out_of_range THEN
          RAISE NOTICE 'migration 059: SKIP queue_items.% (invalid bigint value)', col;
          CONTINUE;
        END;
      END IF;
      EXECUTE format('ALTER TABLE public.queue_items ALTER COLUMN %I DROP DEFAULT', col);
      EXECUTE format('ALTER TABLE public.queue_items ALTER COLUMN %I TYPE BIGINT USING %I::bigint', col, col);
      IF def IS NOT NULL THEN
        EXECUTE format('ALTER TABLE public.queue_items ALTER COLUMN %I SET DEFAULT (%s)::bigint', col, def);
      END IF;
    END IF;
  END LOOP;
END
$migration$;
INSERT INTO schema_version (version, name) VALUES (59, 'queue_items_types')
ON CONFLICT (version) DO NOTHING;
COMMIT;
