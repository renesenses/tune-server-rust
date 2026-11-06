-- 102_credits_source.sql
--
-- #7765 (lot 2, PR 1) : d'où vient un crédit de piste. `track_credits.source`
-- vaut 'manuel', 'musicbrainz', 'balises', 'discogs' ou 'roon'. NULL : ligne
-- ancienne, source inconnue. Chaque écrivain pose sa source et ne purge que
-- ses lignes et les lignes NULL ; les passes « à faire » ignorent 'balises'.
-- Jumelle de la migration SQLite 137.
--
-- Attribution UNE fois des lignes existantes (décision du 03/11) : 'roon' si
-- la piste porte `track_metadata.credits_source = 'roon'`, sinon
-- 'musicbrainz' si `artist_mbid` est renseigné, sinon NULL. Rejouée, elle ne
-- touche que les lignes encore NULL : sans effet sur une ligne déjà attribuée.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS et CREATE INDEX IF NOT EXISTS sont
-- sûrs à rejouer.

BEGIN;

ALTER TABLE track_credits
    ADD COLUMN IF NOT EXISTS source TEXT;

CREATE INDEX IF NOT EXISTS idx_track_credits_track_source
    ON track_credits(track_id, source);

UPDATE track_credits SET source = CASE
    WHEN EXISTS (SELECT 1 FROM track_metadata m WHERE m.track_id = track_credits.track_id
        AND m.key = 'credits_source' AND m.value = 'roon') THEN 'roon'
    WHEN artist_mbid IS NOT NULL AND artist_mbid <> '' THEN 'musicbrainz'
    ELSE NULL END
    WHERE source IS NULL;

INSERT INTO schema_version (version, name) VALUES (102, 'credits_source')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
