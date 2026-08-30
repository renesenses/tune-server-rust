-- 013_numeric_column_types_remaining.sql
--
-- Finish the job started by 010 (albums/tracks) and 011 (listen_history):
-- convert EVERY remaining numeric column that drifted to TEXT back to its
-- intended type, across all tables.
--
-- Root cause (same as 010/011): Postgres databases first created by
-- `tune db migrate-to-postgres` copy SQLite rows with all parameters bound as
-- TEXT, so `pg_migrate.rs::PG_FULL_SCHEMA` declares every column TEXT for copy
-- compatibility. Reads/writes tolerate that (integer→text is an implicit
-- assignment cast), but any *numeric-context* use throws:
--   * `... WHERE year = $int`  → operator does not exist: text = bigint
--     (force-scan album resolution — 22841 write failures, +0 added: forum
--      #1220, tester Jean-François, PostgreSQL backend)
--   * `SUM(duration_ms)`       → function sum(text) does not exist
--   * arithmetic / ORDER BY on a numeric column → wrong or failing.
-- 010 healed the albums/tracks columns the force-scan hits; this migration
-- sweeps the rest so the same class of failure cannot resurface on another
-- column (the exact whack-a-mole that took us from 010 → 011).
--
-- Mechanism mirrors 010 exactly:
--   * Convert ONLY when the column currently exists AND is text/varchar, so it
--     is a strict no-op on any schema already matching the canonical types
--     (fresh installs via 001, and DBs already healed by a previous run).
--   * A value that is not a plain number (unexpected) becomes NULL — or the
--     column's default for NOT NULL columns — rather than aborting the whole
--     migration.
--   * Idempotent and safe to run at every startup (run_pg_migrations).
--
-- Booleans (muted, online, is_current, *_enabled, dlna_*, …) are deliberately
-- LEFT as TEXT: they are bound as PG `boolean` at runtime, so a SMALLINT column
-- would break inserts (`column is of type smallint but expression is of type
-- boolean`). Same reasoning keeps the TEXT `id`/`*_id` keys untouched — the
-- migrated schema uses TEXT ids by design.

BEGIN;

DO $migration$
DECLARE
  c         TEXT[];
  cur_type  TEXT;
  num_re    CONSTANT TEXT := '^-?[0-9]+(\.[0-9]+)?$';
  int_re    CONSTANT TEXT := '^-?[0-9]+$';
  cast_expr TEXT;
  using_x   TEXT;
  -- {table, column, target_type, notnull_fallback}
  -- notnull_fallback = 'NULL' for nullable columns; a literal (e.g. '0') for
  -- NOT NULL columns so a stray non-numeric value cannot break the NOT NULL.
  cols TEXT[][] := ARRAY[
    -- tracks: columns 010 did not cover
    ['tracks','file_mtime','double precision','NULL'],
    ['tracks','acoustid_confidence','double precision','NULL'],
    ['tracks','trailing_silence_ms','integer','NULL'],
    -- ordering / position columns
    ['track_credits','position','integer','NULL'],
    ['playlist_tracks','position','integer','0'],
    ['queue_items','position','integer','0'],
    ['queue_items','duration_ms','bigint','NULL'],
    ['bookmarks','position_ms','bigint','0'],
    -- streaming link confidence (0.0..1.0)
    ['track_source_links','confidence','double precision','0'],
    -- zones: numeric (non-boolean) settings
    -- #2886 : a virgule, pas entier — voir migration 048.
    ['zones','volume','double precision','NULL'],
    ['zones','sync_delay_ms','integer','0'],
    ['zones','last_position_ms','bigint','0'],
    ['zones','max_sample_rate','integer','NULL'],
    -- radio
    ['radio_stations','bitrate','integer','NULL'],
    ['radio_stations','play_count','integer','NULL'],
    -- ratings / smart lists
    ['album_ratings','rating','integer','1'],
    ['smart_playlists','max_tracks','integer','NULL'],
    ['smart_collections','max_limit','integer','NULL'],
    -- alarms
    ['alarms','fade_in_seconds','integer','NULL'],
    ['alarms','fade_duration_s','integer','NULL'],
    ['alarms','volume','double precision','NULL'],
    -- offline cache
    ['offline_cache','file_size','bigint','NULL'],
    ['offline_cache','duration_ms','bigint','NULL']
  ];
BEGIN
  FOREACH c SLICE 1 IN ARRAY cols LOOP
    SELECT data_type INTO cur_type
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];

    -- Only touch columns that exist AND are still text/varchar.
    IF cur_type IN ('text', 'character varying') THEN
      IF c[3] = 'bigint' THEN
        cast_expr := format(
          '(CASE WHEN %1$I ~ %2$L THEN %1$I::bigint END)', c[2], int_re);
      ELSIF c[3] = 'double precision' THEN
        cast_expr := format(
          '(CASE WHEN %1$I ~ %2$L THEN %1$I::double precision END)', c[2], num_re);
      ELSE  -- integer: cast via double precision so '1.0' survives, then trunc
        cast_expr := format(
          '(CASE WHEN %1$I ~ %2$L THEN %1$I::double precision END)::integer',
          c[2], num_re);
      END IF;

      IF c[4] = 'NULL' THEN
        using_x := cast_expr;
      ELSE
        using_x := format('COALESCE(%s, %s)', cast_expr, c[4]);
      END IF;

      EXECUTE format('ALTER TABLE %I ALTER COLUMN %I TYPE %s USING %s',
                     c[1], c[2], c[3], using_x);
      RAISE NOTICE 'migration 013: %.% : % -> %', c[1], c[2], cur_type, c[3];
    END IF;
  END LOOP;
END
$migration$;

INSERT INTO schema_version (version, name)
VALUES (13, 'numeric_column_types_remaining')
ON CONFLICT (version) DO NOTHING;

COMMIT;
