-- 118_albums_release_country.sql
--
-- #8668 : le pays d'EDITION, lu sur la release MusicBrainz de l'album par la
-- passe de fond (`metadata::pays_des_editions`). `release_country` : code
-- ISO 3166-1 alpha-2, '' quand MusicBrainz n'en donne aucun ;
-- `release_country_mbid` : l'identifiant de release dont il a ete lu. Un
-- identifiant change ou retire rend le pays caduc (les facettes exigent
-- `release_country_mbid` = `musicbrainz_release_id`), et la passe le relit.
-- La balise RELEASECOUNTRY d'une piste prime toujours.
-- Jumelle de la migration SQLite 152.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sur a rejouer.

BEGIN;

ALTER TABLE albums ADD COLUMN IF NOT EXISTS release_country TEXT;
ALTER TABLE albums ADD COLUMN IF NOT EXISTS release_country_mbid TEXT;

INSERT INTO schema_version (version, name) VALUES (118, 'albums_release_country')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
