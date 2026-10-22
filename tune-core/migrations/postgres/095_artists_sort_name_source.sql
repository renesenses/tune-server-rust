-- 095_artists_sort_name_source.sql
--
-- #6474, point 2 : d'où vient le nom de tri d'un artiste. 'musicbrainz' quand
-- il a été tiré du sort-name MusicBrainz, NULL sinon (balise ARTISTSORT /
-- ALBUMARTISTSORT, saisie dans « Modifier l'artiste », ou existant). Une
-- balise remplace un nom de tri venu de MusicBrainz, jamais une saisie.
-- Jumelle de la migration SQLite 131.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE artists
    ADD COLUMN IF NOT EXISTS sort_name_source TEXT;

INSERT INTO schema_version (version, name) VALUES (95, 'artists_sort_name_source')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
