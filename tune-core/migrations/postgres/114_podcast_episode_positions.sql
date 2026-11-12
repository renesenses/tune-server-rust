-- 114_podcast_episode_positions.sql
--
-- renesenses/tune-server-rust#8170 (suite de #8152 et de tune-web-client#2730) :
-- la position de CHAQUE episode de podcast commence, une ligne par episode et
-- par zone (`episode_key` = adresse du media sans fragment). Jumelle de la
-- migration SQLite 148.
--
-- `listening_resumes` n'en garde qu'une par podcast et par zone : lancer un
-- autre episode du meme podcast ecrasait la reprise du precedent. Naît vide ;
-- copiee a la bascule apres les scripts (`MIGRATION_TABLES_APRES_SCRIPTS`).
-- Idempotent.

BEGIN;

CREATE TABLE IF NOT EXISTS podcast_episode_positions (
 zone_id BIGINT NOT NULL,
 episode_key TEXT NOT NULL,
 audio_url TEXT NOT NULL,
 position_ms BIGINT NOT NULL DEFAULT 0,
 duration_ms BIGINT NOT NULL DEFAULT 0,
 updated_at TEXT NOT NULL,
 PRIMARY KEY (zone_id, episode_key)
);

INSERT INTO schema_version (version, name) VALUES (114, 'podcast_episode_positions')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
