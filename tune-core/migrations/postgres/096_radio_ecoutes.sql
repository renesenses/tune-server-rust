-- 096_radio_ecoutes.sql
--
-- #6682 : « Mon écoute » compte le temps d'écoute RADIO par station, à part
-- des classements de titres et d'artistes. Une ligne par jour (AAAA-MM-JJ),
-- adresse du flux, zone et profil propriétaire de la session (0 : aucun),
-- incrémentée par la veille d'écoute. Naît vide.
-- Jumelle de la migration SQLite 132.
--
-- Idempotent : CREATE ... IF NOT EXISTS est sûr à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS radio_ecoutes (
    jour TEXT NOT NULL,
    station_url TEXT NOT NULL,
    station_name TEXT NOT NULL DEFAULT '',
    zone_id BIGINT NOT NULL DEFAULT 0,
    profile_id BIGINT NOT NULL DEFAULT 0,
    listening_ms BIGINT NOT NULL DEFAULT 0,
    sessions BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (jour, station_url, zone_id, profile_id)
);
CREATE INDEX IF NOT EXISTS idx_radio_ecoutes_jour ON radio_ecoutes(jour);

INSERT INTO schema_version (version, name) VALUES (96, 'radio_ecoutes')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
