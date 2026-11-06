-- 101_imported_plays.sql
--
-- #7763 : les écoutes IMPORTÉES (Roon, Plex, liste m3u), une ligne par
-- piste, à part de `listen_history` : un import ne fabrique aucune fausse
-- entrée d'historique. Le nombre affiché d'une piste est la somme des deux.
-- `last_played_at` au format de `listen_history.listened_at`
-- (AAAA-MM-JJTHH:MM:SSZ). Naît vide.
-- Jumelle de la migration SQLite 136.
--
-- Idempotent : CREATE ... IF NOT EXISTS est sûr à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS imported_plays (
    track_id BIGINT PRIMARY KEY,
    play_count BIGINT NOT NULL DEFAULT 0,
    last_played_at TEXT
);

INSERT INTO schema_version (version, name) VALUES (101, 'imported_plays')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
