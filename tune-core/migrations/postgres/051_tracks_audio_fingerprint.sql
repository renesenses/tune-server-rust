-- 051_tracks_audio_fingerprint.sql
--
-- BIB-B2 : l'empreinte du CONTENU audio décodé, par piste.
--
-- `audio_hash` hache 64 Ko d'octets du conteneur et ne reconnaît que la
-- copie exacte : deux encodages d'un même master (FLAC et AAC, AIFF et ALAC,
-- 44,1/16 et 96/24) passent pour deux morceaux. L'empreinte se calcule sur
-- le son décodé (mono 11 025 Hz, enveloppe et passages par zéro par trame
-- de 100 ms sur 60 s) et se compare avec tolérance. Versionnée dans la
-- valeur (`env100ms-v1:<hex>`), NULL pour les lignes existantes : la passe
-- ReplayGain et un rattrapage borné la posent.
--
-- TEXT comme côté SQLite : rien à rattraper dans la parité de types.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE tracks
    ADD COLUMN IF NOT EXISTS audio_fingerprint TEXT;

INSERT INTO schema_version (version, name) VALUES (51, 'tracks_audio_fingerprint')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
