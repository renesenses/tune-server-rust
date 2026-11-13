-- 115_sources_reseau.sql
--
-- #8189 : jumelle de la migration SQLite 149 (même DDL,
-- tune-core/migrations/sources_reseau.sql). Idempotent.

BEGIN;

CREATE TABLE IF NOT EXISTS sources_reseau (
 id TEXT PRIMARY KEY,
 type_source TEXT NOT NULL,
 nom TEXT NOT NULL,
 hote TEXT NOT NULL DEFAULT '',
 port BIGINT,
 partage TEXT NOT NULL DEFAULT '',
 racine TEXT NOT NULL DEFAULT '',
 identifiants_scelles TEXT,
 etat TEXT NOT NULL DEFAULT 'nouvelle',
 derniere_erreur TEXT,
 dernier_scan TEXT,
 reprise_scan TEXT,
 cree_le TEXT NOT NULL
)
;

INSERT INTO schema_version (version, name) VALUES (115, 'sources_reseau')
ON CONFLICT (version) DO NOTHING;

COMMIT;
