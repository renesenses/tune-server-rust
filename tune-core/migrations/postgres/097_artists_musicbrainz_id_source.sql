-- 097_artists_musicbrainz_id_source.sql
--
-- #6463 (décision du 23/10, A) : d'où vient le MBID d'un artiste.
-- 'balise' (fichiers, au scan), 'nom' (recherche MusicBrainz du nom),
-- 'pressage' (crédit d'un pressage identifié), 'manuel' (saisi ou détaché
-- par l'utilisateur). NULL : inconnue — tout MBID antérieur à la colonne.
-- Un MBID 'manuel' n'est jamais retiré ni remplacé d'office.
-- Jumelle de la migration SQLite 133.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE artists
    ADD COLUMN IF NOT EXISTS musicbrainz_id_source TEXT;

INSERT INTO schema_version (version, name) VALUES (97, 'artists_musicbrainz_id_source')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
