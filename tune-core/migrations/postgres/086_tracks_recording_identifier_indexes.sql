-- 086_tracks_recording_identifier_indexes.sql
--
-- #2264 : les index des identifiants d'ENREGISTREMENT d'une piste, l'ISRC et
-- le MBID d'enregistrement, sous la forme pliée que comparent le regroupement
-- des versions et la règle de lecture. Jumelle de la migration SQLite 122.
--
-- Index d'EXPRESSION : les requêtes comparent
-- `UPPER(REPLACE(REPLACE(isrc, '-', ''), ' ', ''))` et
-- `LOWER(TRIM(musicbrainz_recording_id))`. PostgreSQL ne sert un index
-- d'expression qu'à l'expression identique (voir `SQL_ISRC_PLIE` et
-- `SQL_MBID_PLIE` dans `tune-core/src/db/migrations.rs`).
--
-- Numérotée 086 : la 084 est la dernière sur le lot le 07/10, et la 085 est
-- prise par #5959 (crête vraie), en PR. Elle EXIGE la 085 avant elle :
-- ordre de fusion #5959 → celle-ci.
--
-- Idempotent : CREATE INDEX IF NOT EXISTS est sûr à rejouer.

BEGIN;

CREATE INDEX IF NOT EXISTS idx_tracks_isrc_norm
    ON tracks ((UPPER(REPLACE(REPLACE(isrc, '-', ''), ' ', ''))));

CREATE INDEX IF NOT EXISTS idx_tracks_mbid_recording_norm
    ON tracks ((LOWER(TRIM(musicbrainz_recording_id))));

INSERT INTO schema_version (version, name) VALUES (86, 'tracks_recording_identifier_indexes')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
