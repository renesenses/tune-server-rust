-- 022_zone_lyrics_offset.sql
--
-- Décalage des paroles synchronisées, par zone, en millisecondes.
--
-- Positif = paroles retardées. Compense la latence entre le moment où le
-- serveur apprend le titre en cours et celui où l'auditeur l'entend (tampon
-- de Tune, puis tampon du renderer). Sur une radio, la position des paroles
-- est l'âge de la métadonnée : la latence se voit donc directement, les
-- paroles défilent en avance (forum #1328).
--
-- Par zone, la profondeur du tampon appartenant à l'appareil. Distinct de
-- sync_delay_ms, qui décale l'AUDIO pour aligner deux pièces.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE zones
    ADD COLUMN IF NOT EXISTS lyrics_offset_ms INTEGER NOT NULL DEFAULT 0;

INSERT INTO schema_version (version, name) VALUES (22, 'zone_lyrics_offset')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
