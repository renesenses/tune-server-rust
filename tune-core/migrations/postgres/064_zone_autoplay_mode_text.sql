-- #2271: six named AutoPlay modes share the historical boolean column.
-- Native migration 032 used INTEGER, SQLite imports already use TEXT.
-- Preserve 0/1, NULL and opaque names without narrowing or deleting rows.
BEGIN;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS autoplay_enabled TEXT DEFAULT '0';
ALTER TABLE zones ALTER COLUMN autoplay_enabled DROP DEFAULT;
ALTER TABLE zones ALTER COLUMN autoplay_enabled TYPE TEXT USING autoplay_enabled::TEXT;
ALTER TABLE zones ALTER COLUMN autoplay_enabled SET DEFAULT '0';
INSERT INTO schema_version (version, name) VALUES (64, 'zone_autoplay_mode_text')
ON CONFLICT (version) DO NOTHING;
COMMIT;
