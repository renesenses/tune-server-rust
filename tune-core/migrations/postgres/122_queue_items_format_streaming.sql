-- #8788 — format des pistes UPnP en file, inconnu pour les anciennes lignes.
-- Jumelle SQLite 155.
BEGIN;
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS media_format TEXT;
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS sample_rate INTEGER;
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS bit_depth INTEGER;

INSERT INTO schema_version (version, name) VALUES (122, 'queue_items_format_streaming')
    ON CONFLICT (version) DO NOTHING;
COMMIT;
