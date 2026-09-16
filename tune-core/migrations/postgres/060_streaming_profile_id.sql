-- #3715: the repository binds profile_id as i64 on both database engines.
-- Repair existing native installs whose table arrived after migration 012.
-- Only this column changes: service IDs and manual positions remain TEXT.
-- Unexpected legacy values abort this transaction without changing data or
-- recording version 60; do not silently leave a TEXT column with i64 writers.
BEGIN;
DO $migration$
DECLARE
  typ TEXT;
  def TEXT;
BEGIN
  SELECT data_type, column_default INTO typ, def
    FROM information_schema.columns
   WHERE table_schema = 'public' AND table_name = 'streaming_favorites'
     AND column_name = 'profile_id';
  IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
    ALTER TABLE public.streaming_favorites ALTER COLUMN profile_id DROP DEFAULT;
    ALTER TABLE public.streaming_favorites ALTER COLUMN profile_id TYPE BIGINT
      USING profile_id::bigint;
    IF def IS NOT NULL THEN
      EXECUTE format('ALTER TABLE public.streaming_favorites ALTER COLUMN profile_id SET DEFAULT (%s)::bigint', def);
    END IF;
  END IF;
END
$migration$;
INSERT INTO schema_version (version, name) VALUES (60, 'streaming_profile_id')
ON CONFLICT (version) DO NOTHING;
COMMIT;
