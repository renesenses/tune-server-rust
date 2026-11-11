-- 110_radio_stations_surcharge_locale.sql
--
-- web#2795 : la SURCHARGE LOCALE d'une station de « Mes radios » liée à sa
-- fiche de l'annuaire. `surcharge_locale` porte la date (RFC 3339) à laquelle
-- l'utilisateur a changé chez lui le nom, le logo ou le flux ; NULL : la
-- station suit sa fiche. Le relevé de l'annuaire ne touche pas une station
-- surchargée ; « Revenir à la fiche de l'annuaire » remet NULL.
-- Jumelle de la migration SQLite 144.
--
-- TEXT comme côté SQLite : la copie de bascule n'a rien à convertir.
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE radio_stations
    ADD COLUMN IF NOT EXISTS surcharge_locale TEXT;

INSERT INTO schema_version (version, name) VALUES (110, 'radio_stations_surcharge_locale')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
