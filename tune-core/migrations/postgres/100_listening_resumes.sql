CREATE TABLE IF NOT EXISTS listening_resumes (
 resume_key TEXT PRIMARY KEY,
 zone_id BIGINT NOT NULL,
 context_type TEXT NOT NULL,
 context_id TEXT NOT NULL,
 source TEXT NOT NULL,
 payload TEXT NOT NULL,
 updated_at TEXT NOT NULL,
 UNIQUE(zone_id, context_type, context_id, source)
);

INSERT INTO schema_version (version, name) VALUES (100, 'listening_resumes')
ON CONFLICT (version) DO NOTHING;
