-- #7619 / #7656 : même clé et même index que la passe de démarrage.
-- L'ordre des octets doit rester identique à SQLite et aux curseurs Rust.
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cle_tri TEXT COLLATE "C";
CREATE INDEX IF NOT EXISTS idx_tracks_cle_tri ON tracks(cle_tri, id);

INSERT INTO schema_version (version, name) VALUES (101, 'tracks_cle_tri')
ON CONFLICT (version) DO NOTHING;
