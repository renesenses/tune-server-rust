-- 090_radio_stations_external_id.sql
--
-- #6516 : le lien d'une station de « Mes radios » avec sa fiche de l'annuaire
-- radio-metadata.fr (`S000973`). Jumelle de la migration SQLite 126.
--
-- TEXT comme côté SQLite ; NULL pour l'existant : le rapprochement de départ
-- le remplit au premier relevé de l'annuaire.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS et CREATE INDEX IF NOT EXISTS sont
-- sûrs à rejouer.

BEGIN;

ALTER TABLE radio_stations
    ADD COLUMN IF NOT EXISTS external_id TEXT;

CREATE INDEX IF NOT EXISTS idx_radio_stations_external_id ON radio_stations(external_id);

INSERT INTO schema_version (version, name) VALUES (90, 'radio_stations_external_id')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
