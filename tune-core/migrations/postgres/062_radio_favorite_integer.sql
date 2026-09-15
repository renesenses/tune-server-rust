-- #3715: RadioRepo now binds integer 0/1, matching set_favorite.
-- The previous boolean writer stored true/false on TEXT imports, making a
-- favorite invisible to both the integer reader and WHERE is_favorite = '1'.
-- Preserve numeric legacy values, nulls, rows, constraints and the default.
-- Invalid/out-of-range data aborts without recording the migration.
BEGIN;
DO $migration$
DECLARE
  typ TEXT;
  def TEXT;
BEGIN
  SELECT data_type, column_default INTO typ, def FROM information_schema.columns
   WHERE table_schema = 'public' AND table_name = 'radio_stations'
     AND column_name = 'is_favorite';
  IF typ IN ('smallint', 'integer', 'text', 'character varying', 'boolean') THEN
    ALTER TABLE public.radio_stations ALTER COLUMN is_favorite DROP DEFAULT;
    ALTER TABLE public.radio_stations ALTER COLUMN is_favorite TYPE BIGINT USING
      (CASE lower(btrim(is_favorite::text)) WHEN 'true' THEN '1'
        WHEN 'false' THEN '0' ELSE is_favorite::text END)::bigint;
    IF def IS NOT NULL THEN
      EXECUTE format('ALTER TABLE public.radio_stations ALTER COLUMN is_favorite SET DEFAULT (CASE lower(btrim((%s)::text)) WHEN ''true'' THEN ''1'' WHEN ''false'' THEN ''0'' ELSE (%s)::text END)::bigint', def, def);
    END IF;
  END IF;
END
$migration$;
INSERT INTO schema_version (version, name) VALUES (62, 'radio_favorite_integer')
ON CONFLICT (version) DO NOTHING;
COMMIT;
