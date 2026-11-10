-- 110_artists_country.sql
--
-- #8090 : le pays de l'ARTISTE, lu sur sa fiche MusicBrainz par la passe de
-- fond (`metadata::pays_des_artistes`). `country` : code ISO 3166-1 alpha-2,
-- '' quand MusicBrainz n'en donne aucun ; `country_mbid` : le MBID dont il a
-- ete lu. Un MBID change ou retire rend le pays caduc (les facettes exigent
-- `country_mbid` = `musicbrainz_id`), et la passe le relit.
-- Jumelle de la migration SQLite 144.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sur a rejouer.

BEGIN;

ALTER TABLE artists ADD COLUMN IF NOT EXISTS country TEXT;
ALTER TABLE artists ADD COLUMN IF NOT EXISTS country_mbid TEXT;

INSERT INTO schema_version (version, name) VALUES (110, 'artists_country')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
