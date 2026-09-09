-- 054_zones_output_endpoint_id.sql
--
-- #2269 — l'identifiant d'endpoint STABLE d'une sortie locale, par zone.
--
-- `zones.output_device_id` vaut `local:{nom du périphérique}` : l'identité
-- d'une zone locale est son NOM. Windows renomme l'endpoint au changement de
-- taux d'échantillonnage, et la zone ne retrouve plus rien. L'identifiant qui
-- traverse un renommage existe depuis #2403 (`AudioDevice::endpoint_id`,
-- capturé à la découverte et lu en premier par `resolve_device`) mais rien ne
-- le persistait.
--
-- La colonne ne REMPLACE pas `output_device_id` : celui-ci reste l'identité de
-- la zone, et tout ce qui s'y accroche — réglages, file, volume, index unique
-- partiel — reste en place. Aucune ligne existante n'est modifiée : NULL veut
-- dire « pas encore appris », jamais « inconnu donc n'importe lequel ». La
-- valeur n'est écrite qu'au moment où l'énumération montre l'appareil sous le
-- nom que la zone porte DÉJÀ.
--
-- TEXT comme côté SQLite (migration 98) : rien à rattraper dans la parité de
-- types PostgreSQL.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.
BEGIN;

ALTER TABLE zones
    ADD COLUMN IF NOT EXISTS output_endpoint_id TEXT;

INSERT INTO schema_version (version, name) VALUES (54, 'zones_output_endpoint_id')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
