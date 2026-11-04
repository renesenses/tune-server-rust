CREATE TABLE IF NOT EXISTS cles_clients (
 id TEXT PRIMARY KEY,
 profile_id BIGINT NOT NULL,
 nom TEXT NOT NULL,
 empreinte_sha256 TEXT NOT NULL UNIQUE,
 cree_le TEXT NOT NULL,
 revoquee_le TEXT
);

INSERT INTO schema_version (version, name) VALUES (101, 'cles_clients')
ON CONFLICT (version) DO NOTHING;
