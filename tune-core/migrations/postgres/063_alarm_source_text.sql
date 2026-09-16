-- Service identifiers are opaque strings, as written by the alarm API.
-- Preserve all existing IDs and NULLs; no numeric narrowing or row deletion.
BEGIN;
DO $migration$
DECLARE typ TEXT;
BEGIN
 SELECT data_type INTO typ FROM information_schema.columns
 WHERE table_schema='public' AND table_name='alarms' AND column_name='source_id';
 IF typ IN ('smallint','integer','bigint') THEN
  ALTER TABLE public.alarms ALTER COLUMN source_id TYPE TEXT USING source_id::text;
 END IF;
END
$migration$;
INSERT INTO schema_version (version, name) VALUES (63, 'alarm_source_text')
ON CONFLICT (version) DO NOTHING;
COMMIT;
