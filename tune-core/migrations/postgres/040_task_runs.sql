-- task_runs : registre des executions automatisees (#2080).
--
-- Jumelle PostgreSQL de la migration SQLite 87. Les deux listes sont SEPAREES :
-- ecrite d'un seul cote, la table manquerait a tout le parc PostgreSQL (.15,
-- .18, Docker) et la route d'observabilite y rendrait une erreur SQL.
--
-- Une vingtaine de passes tournent seules dans Tune (scan de demarrage,
-- ReplayGain, enrichissement, battement de coeur, nettoyages). Aucune ne
-- laissait de trace interrogeable : le journal defile, et « ca n'a rien fait »
-- restait indecidable.
--
-- PAS DE COLONNE `id` : la cle naturelle (boot_id, task, seq) EST la cle
-- primaire. Meme choix que `favorite_facets` (migration PG 038) et pour la
-- meme raison — une colonne `id` impose la divergence AUTOINCREMENT /
-- BIGSERIAL que la bascule SQLite -> PostgreSQL a deja payee cher (#1706).
--
-- Types : tout en TEXT sauf les deux compteurs. `seq` et `duration_ms` sont
-- des BIGINT, et cette table n'est PAS dans `MIGRATION_TABLES` de
-- `pg_migrate.rs` — elle n'est donc jamais remplie par la copie SQLite -> PG
-- qui lie toute valeur en texte, et n'a pas le probleme de la colonne creee en
-- TEXT que la migration 012 repare pour les tables copiees.
--
-- Ne PAS copier ce registre a la bascule est deliberé : ses `boot_id`
-- designent des incarnations d'un processus qui tournait sur l'autre moteur.
-- L'historique d'observabilite recommence avec le nouveau moteur, et c'est la
-- lecture honnete.
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS task_runs (
    boot_id TEXT NOT NULL,
    task TEXT NOT NULL,
    seq BIGINT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    duration_ms BIGINT,
    outcome TEXT NOT NULL,
    items BIGINT,
    detail TEXT,
    PRIMARY KEY (boot_id, task, seq)
);

CREATE INDEX IF NOT EXISTS idx_task_runs_task_started ON task_runs(task, started_at);
CREATE INDEX IF NOT EXISTS idx_task_runs_outcome ON task_runs(outcome);
