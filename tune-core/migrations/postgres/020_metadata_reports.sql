-- 020_metadata_reports.sql
--
-- Signalements de métadonnées (SQLite migration v69). Le seul signalement
-- existant (image d'artiste) squattait la table settings — impossible à
-- lister, agréger ou pousser au cloud. Servi par POST/GET /api/v1/library/reports.

BEGIN;

CREATE TABLE IF NOT EXISTS metadata_reports (
    id BIGSERIAL PRIMARY KEY,
    entity TEXT NOT NULL,
    entity_id BIGINT,
    mbid TEXT,
    field TEXT,
    value TEXT,
    reason TEXT NOT NULL,
    comment TEXT,
    created_at TEXT NOT NULL,
    pushed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_metadata_reports_entity ON metadata_reports(entity, entity_id);

INSERT INTO schema_version (version, name) VALUES (20, 'metadata_reports')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
