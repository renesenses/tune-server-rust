-- 012_integer_id_columns.sql
--
-- Restore auto-incrementing INTEGER primary keys on databases created by the
-- SQLite->PG `migrate-to-postgres` path.
--
-- `pg_migrate.rs` created every `id` as `TEXT PRIMARY KEY` (no sequence),
-- inheriting SQLite's dynamic typing, whereas a fresh PG install created by
-- 001_initial_schema.sql uses `id BIGSERIAL PRIMARY KEY`. Because every
-- CREATE TABLE uses IF NOT EXISTS, 001 never repaired a migrated schema, and
-- 010/011 only touched non-id numeric columns. The rows copied by the data
-- migration carry explicit ids, so they read fine — but every NEW app insert
-- OMITS `id` (expecting auto-generation, like SQLite), so on such a schema it
-- inserts NULL and fails:
--   null value in column "id" of relation "tracks" violates not-null constraint
-- => a migrated database can no longer add tracks at all (JF, v0.9.2x: a scan
-- saw 45k files but wrote 0, "22841 échecs d'écriture en base").
--
-- This converts our integer primary keys AND the integer foreign-key columns
-- that join to them from TEXT back to BIGINT, and attaches a sequence to each
-- primary key set past its current MAX(id) — matching what BIGSERIAL gives a
-- fresh install. Only STRING identifier columns (streaming source_id,
-- musicbrainz_*, device ids, polymorphic/streaming refs) are left as TEXT.
--
-- Safety:
--   * Idempotent: a column is only touched while it is still text/varchar, so
--     this is a no-op on a fresh (already-bigint) schema.
--   * Guarded cast: a column is converted ONLY when every non-null value is an
--     integer literal. Anything else (e.g. the legacy `lq_`/`sq_`-prefixed
--     `queue_items.id`) is left untouched with a NOTICE, so the migration can
--     never abort mid-way on unexpected data.
--   * No FK constraints exist in the migrated schema, so no constraint juggling
--     is needed and column order is irrelevant.

BEGIN;

DO $migration$
DECLARE
  int_re CONSTANT TEXT := '^-?[0-9]+$';
  -- Tables whose `id` primary key must become BIGINT + sequence.
  pk_tables TEXT[] := ARRAY[
    'artists','albums','tracks','track_credits','playlists','playlist_tracks',
    'zones','play_queue','streaming_queue','queue_items','listen_history',
    'radio_stations','radio_favorites','profiles','favorites',
    'streaming_favorites','tags','item_tags','album_ratings','smart_playlists',
    'smart_collections','bookmarks','alarms','network_mounts',
    'podcast_subscriptions','offline_cache','sync_links','sync_link_snapshots',
    'track_source_links'
  ];
  -- Integer foreign-key / reference columns that join to a bigint id above.
  fk_cols TEXT[][] := ARRAY[
    ['albums','artist_id'],
    ['tracks','album_id'], ['tracks','artist_id'],
    ['track_credits','track_id'], ['track_credits','artist_id'],
    ['track_metadata','track_id'],
    ['playlists','profile_id'],
    ['playlist_tracks','playlist_id'], ['playlist_tracks','track_id'],
    ['zones','last_track_id'], ['zones','dsp_preset_id'],
    ['play_queue','zone_id'], ['play_queue','track_id'],
    ['streaming_queue','zone_id'],
    ['queue_items','zone_id'], ['queue_items','track_id'],
    ['listen_history','track_id'], ['listen_history','zone_id'],
    ['listen_history','album_id'], ['listen_history','profile_id'],
    ['favorites','profile_id'], ['favorites','item_id'],
    ['streaming_favorites','profile_id'],
    ['item_tags','tag_id'], ['item_tags','item_id'],
    ['album_ratings','album_id'], ['album_ratings','profile_id'],
    ['bookmarks','track_id'],
    ['alarms','zone_id'],
    ['sync_links','local_playlist_id'],
    ['sync_link_snapshots','playlist_link_id'],
    ['track_source_links','track_id']
  ];
  t        TEXT;
  c        TEXT[];
  cur_type TEXT;
  col_def  TEXT;
  bad      BIGINT;
  seq      TEXT;
  maxid    BIGINT;
  target_tables TEXT[];
  trg      RECORD;
  trg_defs TEXT[] := ARRAY[]::TEXT[];
  tdef     TEXT;
BEGIN
  -- Build the set of tables this migration alters (pk ids + fk columns).
  target_tables := pk_tables;
  FOREACH c SLICE 1 IN ARRAY fk_cols LOOP
    IF NOT (c[1] = ANY(target_tables)) THEN
      target_tables := array_append(target_tables, c[1]);
    END IF;
  END LOOP;

  -- PG refuses `ALTER COLUMN ... TYPE` on a column referenced by a trigger
  -- definition. The FTS `*_search_tsv_trg` triggers (migration 002) list
  -- id/artist_id/album_id in their `UPDATE OF` column set, so the very first
  -- ALTER aborts the whole migration -> transactional rollback -> it re-runs
  -- forever, bricking startup on EVERY SQLite->PG migrated database (JF,
  -- v0.9.26). Capture every non-internal trigger on the target tables, drop
  -- them, run the conversions, then recreate them verbatim from
  -- pg_get_triggerdef so their behaviour is byte-for-byte unchanged.
  FOR trg IN
    SELECT cl.relname AS tbl, t.tgname AS name, pg_get_triggerdef(t.oid) AS def
      FROM pg_trigger t
      JOIN pg_class cl ON cl.oid = t.tgrelid
     WHERE NOT t.tgisinternal
       AND cl.relname = ANY(target_tables)
  LOOP
    trg_defs := array_append(trg_defs, trg.def);
    EXECUTE format('DROP TRIGGER %I ON %I', trg.name, trg.tbl);
  END LOOP;

  -- 1) Integer FK columns: cast text -> bigint (no sequence).
  FOREACH c SLICE 1 IN ARRAY fk_cols LOOP
    SELECT data_type, column_default INTO cur_type, col_def
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];
    IF cur_type IN ('text', 'character varying') THEN
      EXECUTE format(
        'SELECT count(*) FROM %I WHERE %I IS NOT NULL AND %I !~ %L',
        c[1], c[2], c[2], int_re) INTO bad;
      IF bad = 0 THEN
        -- A text default (e.g. profile_id DEFAULT '1') can't be cast to bigint
        -- during ALTER TYPE, so drop it first and restore it as an integer
        -- afterwards (keeps NOT NULL columns insertable).
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
        RAISE NOTICE 'migration 012: %.% text->bigint', c[1], c[2];
      ELSE
        RAISE NOTICE 'migration 012: SKIP %.% (% non-integer values)',
          c[1], c[2], bad;
      END IF;
    END IF;
  END LOOP;

  -- 2) Primary-key `id` columns: cast + sequence set past MAX(id).
  FOREACH t IN ARRAY pk_tables LOOP
    SELECT data_type INTO cur_type
      FROM information_schema.columns
     WHERE table_name = t AND column_name = 'id';
    IF cur_type IN ('text', 'character varying') THEN
      EXECUTE format(
        'SELECT count(*) FROM %I WHERE id IS NOT NULL AND id !~ %L', t, int_re)
        INTO bad;
      IF bad = 0 THEN
        -- Drop any existing default first (zones / streaming_favorites carry a
        -- `nextval(...)::text` default that would be invalid once id is bigint).
        EXECUTE format('ALTER TABLE %I ALTER COLUMN id DROP DEFAULT', t);
        EXECUTE format('ALTER TABLE %I ALTER COLUMN id TYPE bigint USING id::bigint', t);
        seq := t || '_id_seq';
        EXECUTE format('CREATE SEQUENCE IF NOT EXISTS %I', seq);
        EXECUTE format('SELECT COALESCE(MAX(id), 0) FROM %I', t) INTO maxid;
        -- setval(seq, MAX)  → next nextval = MAX+1 (is_called=true);
        -- empty table       → next nextval = 1     (is_called=false).
        EXECUTE format('SELECT setval(%L, GREATEST(%s, 1), %s)',
          seq, maxid, CASE WHEN maxid > 0 THEN 'true' ELSE 'false' END);
        EXECUTE format('ALTER TABLE %I ALTER COLUMN id SET DEFAULT nextval(%L)', t, seq);
        -- Link the id column to its sequence. On installs whose schema ownership
        -- has drifted (some tables owned by a different role than the migration's
        -- connection role — e.g. tuneserver- vs tune-owned tables on prod .15),
        -- Postgres rejects OWNED BY with "sequence must have same owner as table
        -- it is linked to", which used to abort the whole migration and crash-loop
        -- the server on boot. Align the sequence owner to the table's first, and
        -- make the link best-effort: OWNED BY only governs cascade-drop (which
        -- never happens for these core id columns), while the column DEFAULT set
        -- just above is what actually matters. A residual mismatch is skipped, not
        -- fatal.
        BEGIN
          EXECUTE (
            SELECT format('ALTER SEQUENCE %I OWNER TO %I', seq, tableowner)
              FROM pg_tables
             WHERE schemaname = current_schema() AND tablename = t
          );
          EXECUTE format('ALTER SEQUENCE %I OWNED BY %I.id', seq, t);
        EXCEPTION WHEN OTHERS THEN
          RAISE NOTICE 'migration 012: OWNED BY skipped for %.id (%)', t, SQLERRM;
        END;
        RAISE NOTICE 'migration 012: %.id text->bigint + seq (next %)', t, maxid + 1;
      ELSE
        RAISE NOTICE 'migration 012: SKIP %.id (% non-integer values)', t, bad;
      END IF;
    END IF;
  END LOOP;

  -- Recreate the triggers now the columns are bigint (dependency re-attaches).
  FOREACH tdef IN ARRAY trg_defs LOOP
    EXECUTE tdef;
  END LOOP;
END
$migration$;

INSERT INTO schema_version (version, name)
VALUES (12, 'integer_id_columns')
ON CONFLICT (version) DO NOTHING;

COMMIT;
