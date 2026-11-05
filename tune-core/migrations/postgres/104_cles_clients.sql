CREATE TABLE IF NOT EXISTS cles_clients (
 id TEXT PRIMARY KEY,
 profile_id BIGINT NOT NULL,
 nom TEXT NOT NULL,
 empreinte_sha256 TEXT NOT NULL UNIQUE,
 cree_le TEXT NOT NULL,
 revoquee_le TEXT
);
CREATE INDEX IF NOT EXISTS idx_cles_clients_profile ON cles_clients(profile_id);

INSERT INTO schema_version (version, name) VALUES (104, 'cles_clients')
ON CONFLICT (version) DO NOTHING;
