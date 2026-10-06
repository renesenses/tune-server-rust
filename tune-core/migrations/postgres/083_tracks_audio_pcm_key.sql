-- 083_tracks_audio_pcm_key.sql
--
-- #5594 (lot 1) : la clé du signal PCM d'une piste FLAC. Jumelle de la
-- migration SQLite 119.
--
-- `audio_pcm_key` = `flac-md5-v1:<md5>:<total_samples>:<sample_rate>:
-- <channels>:<bits>`, tirée du MD5 des échantillons que porte STREAMINFO.
-- NULL pour un MD5 nul, une piste CUE, tout ce qui n'est pas du FLAC.
-- `audio_pcm_key_seen` = l'`audio_hash` de l'état du fichier dont l'en-tête a
-- été lu ; NULL = jamais lu. La passe `taches_de_fond::cle_pcm` remplit les
-- deux, en fond, par l'en-tête seulement.
--
-- TEXT comme côté SQLite ; NULL pour l'existant.
--
-- Numérotée 083 : la 081 est sur le lot (#5822), la 082 est prise par #5827.
-- Elle EXIGE la 082 avant elle.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS et CREATE INDEX IF NOT EXISTS sont
-- sûrs à rejouer.

BEGIN;

ALTER TABLE tracks
    ADD COLUMN IF NOT EXISTS audio_pcm_key TEXT;

ALTER TABLE tracks
    ADD COLUMN IF NOT EXISTS audio_pcm_key_seen TEXT;

CREATE INDEX IF NOT EXISTS idx_tracks_audio_pcm_key ON tracks(audio_pcm_key);

INSERT INTO schema_version (version, name) VALUES (83, 'tracks_audio_pcm_key')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
