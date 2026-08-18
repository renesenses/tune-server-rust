-- 010_numeric_column_types.sql
--
-- Repair numeric columns on `albums`/`tracks` that drifted to TEXT.
--
-- Postgres databases first created by `tune db migrate-to-postgres` inherited
-- SQLite's dynamic typing: several columns that 001_initial_schema.sql declares
-- as INTEGER/BIGINT/DOUBLE PRECISION were created as TEXT. Normal reads/writes
-- tolerated it, but a *force* library scan resolves albums with
-- `... WHERE year = $int`, which on such a schema throws
-- `operator does not exist: text = bigint` — failing album creation and
-- orphaning tracks (Bertrand, .15: a force-scan left ~25k tracks album-less
-- until `albums.year` was converted back to integer).
--
-- This migration converts every affected column back to its intended type,
-- but ONLY when it is currently text/varchar, so it is a no-op on any schema
-- already matching 001 (fresh installs). Values that are not a plain number
-- (unexpected) become NULL rather than aborting the migration.

BEGIN;

DO $migration$
DECLARE
  c        TEXT[];
  cur_type TEXT;
  num_re   CONSTANT TEXT := '^-?[0-9]+(\.[0-9]+)?$';
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  using_x  TEXT;
  -- {table, column, target_type}
  cols TEXT[][] := ARRAY[
    ['albums','year','integer'],
    ['albums','original_year','integer'],
    ['albums','disc_count','integer'],
    ['albums','track_count','integer'],
    ['albums','sample_rate','integer'],
    ['albums','bit_depth','integer'],
    ['tracks','year','integer'],
    ['tracks','sample_rate','integer'],
    ['tracks','bit_depth','integer'],
    ['tracks','channels','integer'],
    ['tracks','disc_number','integer'],
    ['tracks','track_number','integer'],
    ['tracks','duration_ms','bigint'],
    ['tracks','file_size','bigint'],
    ['tracks','bpm','double precision']
  ];
BEGIN
  FOREACH c SLICE 1 IN ARRAY cols LOOP
    SELECT data_type INTO cur_type
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];

    -- Only touch columns that exist AND are still text/varchar.
    IF cur_type IN ('text', 'character varying') THEN
      IF c[3] = 'bigint' THEN
        using_x := format(
          '(CASE WHEN %1$I ~ %2$L THEN %1$I::bigint END)', c[2], int_re);
      ELSIF c[3] = 'double precision' THEN
        using_x := format(
          '(CASE WHEN %1$I ~ %2$L THEN %1$I::double precision END)', c[2], num_re);
      ELSE  -- integer
        using_x := format(
          '(CASE WHEN %1$I ~ %2$L THEN %1$I::double precision END)::integer',
          c[2], num_re);
      END IF;

      EXECUTE format('ALTER TABLE %I ALTER COLUMN %I TYPE %s USING %s',
                     c[1], c[2], c[3], using_x);
      RAISE NOTICE 'migration 010: % .% : % -> %', c[1], c[2], cur_type, c[3];
    END IF;
  END LOOP;
END
$migration$;

INSERT INTO schema_version (version, name)
VALUES (10, 'numeric_column_types')
ON CONFLICT (version) DO NOTHING;

COMMIT;
