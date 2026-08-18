-- 023_metadata_proposals.sql
--
-- Corrections que la communaute propose sur les metadonnees de cette instance
-- (migration SQLite v74). Elles arrivent de mozaiklabs.fr et attendent la
-- validation de l'utilisateur : `decision` NULL = en attente.
--
-- Local d'abord, comme les signalements : la ligne fait foi, le renvoi de la
-- decision au cloud est un effet de bord au-dessus. Une decision prise hors
-- ligne n'est pas perdue, elle repart au cycle suivant.

BEGIN;

CREATE TABLE IF NOT EXISTS metadata_proposals (
    id BIGSERIAL PRIMARY KEY,
    entity TEXT NOT NULL,
    cloud_entity_id BIGINT NOT NULL,
    local_id BIGINT NOT NULL,
    title TEXT,
    artist TEXT,
    field TEXT NOT NULL,
    current_value TEXT,
    proposed_value TEXT,
    servers_count BIGINT NOT NULL DEFAULT 0,
    fetched_at TEXT NOT NULL,
    decision TEXT,
    decided_at TEXT,
    pushed_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_metadata_proposals_key
    ON metadata_proposals(entity, cloud_entity_id, field);

CREATE INDEX IF NOT EXISTS idx_metadata_proposals_pending
    ON metadata_proposals(decision, servers_count);

INSERT INTO schema_version (version, name) VALUES (23, 'metadata_proposals')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
