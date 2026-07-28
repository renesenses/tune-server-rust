-- 011_history_numeric_column_types.sql
--
-- Repair the numeric column on `listen_history` that drifted to TEXT.
--
-- Same root cause as 010: Postgres databases first created by
-- `tune db migrate-to-postgres` inherited SQLite's dynamic typing, so
-- `listen_history.duration_ms` — declared numeric everywhere it is used — was
-- created as TEXT. 010 converted the drifted columns on `albums`/`tracks` but
-- did NOT cover `listen_history`, so the listening dashboard still crashed with
-- `function sum(text) does not exist` when it runs
-- `SUM(duration_ms) FROM listen_history` (JF, v0.9.21).
--
-- Converts the column back to BIGINT, but ONLY when it is currently
-- text/varchar, so it is a no-op on any schema already matching 001 (fresh
-- installs). Values that are not a plain integer (unexpected) become NULL
-- rather than aborting the migration. Mirrors 010's mechanism exactly.

BEGIN;

DO $migration$
DECLARE
  c        TEXT[];
  cur_type TEXT;
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  using_x  TEXT;
  -- {table, column, target_type}
  cols TEXT[][] := ARRAY[
    ['listen_history','duration_ms','bigint']
  ];
BEGIN
  FOREACH c SLICE 1 IN ARRAY cols LOOP
    SELECT data_type INTO cur_type
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];

    -- Only touch columns that exist AND are still text/varchar.
    IF cur_type IN ('text', 'character varying') THEN
      using_x := format(
        '(CASE WHEN %1$I ~ %2$L THEN %1$I::bigint END)', c[2], int_re);
      EXECUTE format('ALTER TABLE %I ALTER COLUMN %I TYPE %s USING %s',
                     c[1], c[2], c[3], using_x);
      RAISE NOTICE 'migration 011: % .% : % -> %', c[1], c[2], cur_type, c[3];
    END IF;
  END LOOP;
END
$migration$;

INSERT INTO schema_version (version, name)
VALUES (11, 'history_numeric_column_types')
ON CONFLICT (version) DO NOTHING;

COMMIT;
