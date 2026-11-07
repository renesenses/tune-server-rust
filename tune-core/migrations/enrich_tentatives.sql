CREATE TABLE IF NOT EXISTS enrich_tentatives (
 track_id BIGINT PRIMARY KEY,
 issue TEXT NOT NULL,
 echecs BIGINT NOT NULL DEFAULT 0,
 tente_le BIGINT NOT NULL,
 reessai_apres BIGINT NOT NULL DEFAULT 0,
 mtime_vu TEXT
)