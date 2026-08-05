-- 016_alarms_profile_id.sql
--
-- Per-profile alarms (chantier 2 / C2, #1150): SQLite migration v64 added
-- `profile_id` to alarms (add_column_if_missing), but the PG side never got
-- a numbered migration — 008 created the table without it, and the ALTER in
-- pg_migrate.rs only runs during the one-shot SQLite→PG data migration, so
-- databases migrated earlier (.15 prod, v0.9.49) never received the column.
-- The alarm scheduler polls every 30 s and its query selects `profile_id`,
-- so on such a backend the log filled with
--   alarm_scheduler_error ... column "profile_id" does not exist
-- every 30 seconds and scheduled alarms could not run.
--
-- Nullable, no default — legacy alarms have no owner and stay NULL (never
-- guessed), matching the SQLite migration and pg_migrate.rs (BIGINT).
--
-- Idempotent (ADD COLUMN IF NOT EXISTS).

BEGIN;

ALTER TABLE alarms ADD COLUMN IF NOT EXISTS profile_id BIGINT;

INSERT INTO schema_version (version, name) VALUES (16, 'alarms_profile_id')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
