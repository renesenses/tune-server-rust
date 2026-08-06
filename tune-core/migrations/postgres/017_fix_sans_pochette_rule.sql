-- 017_fix_sans_pochette_rule.sql
--
-- The seeded "🖼️ Sans pochette" smart collection carried a placeholder rule
-- (`format is_not_empty` — i.e. every track in the library) instead of an
-- actual no-cover test, so it counted the whole library everywhere (Oxygen
-- `collection` facet, list track_count). The rule engine supports
-- `cover_path is_empty`; point the seed at it. PG databases got the
-- placeholder through the one-shot SQLite→PG data migration (PG never seeds
-- smart collections itself), so they need the same correction as SQLite
-- migration v66.
--
-- Guarded on the exact placeholder rules string so a user-customized
-- collection is never touched; idempotent by the same guard.

BEGIN;

UPDATE smart_collections
SET rules = '[{"field":"cover_path","operator":"is_empty","value":""}]'
WHERE name LIKE '%pochette%'
  AND rules = '[{"field":"format","operator":"is_not_empty","value":""}]';

INSERT INTO schema_version (version, name) VALUES (17, 'fix_sans_pochette_rule')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
