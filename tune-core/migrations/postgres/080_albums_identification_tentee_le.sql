-- 080_albums_identification_tentee_le.sql
--
-- #4991 (b) : la marque « déjà tenté, rien trouvé » de l'identification en lot
-- (`POST /library/identify-all`). Jumelle de la migration SQLite 116.
--
-- Un album que MusicBrainz n'a pas ne reçoit pas de `musicbrainz_release_id`
-- et restait en tête de la sélection `ORDER BY al.id` : la relance réattaquait
-- l'amas qui venait d'échouer. La sélection range désormais les albums jamais
-- tentés en tête, puis les tentés du plus ancien au plus récent.
--
-- TEXT ISO-8601 UTC comme côté SQLite ; NULL pour l'existant (= jamais tenté).
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS identification_tentee_le TEXT;

INSERT INTO schema_version (version, name) VALUES (80, 'albums_identification_tentee_le')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
