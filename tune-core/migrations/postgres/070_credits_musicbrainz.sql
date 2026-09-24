-- Les CREDITS MusicBrainz par disque (#4767).
--
-- Jumelle de la migration SQLite 107. Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posee d'un
-- seul cote ne repare que la moitie du parc (#1612, #2111).
--
-- Deux colonnes, toutes deux TEXT et sans defaut, sur les deux moteurs :
--
-- * `track_credits.artist_mbid` — l'identifiant MusicBrainz de l'artiste
--   CREDITE. La page artiste le compare a `artists.musicbrainz_id` : un
--   musicien de seance n'a souvent aucune fiche locale au moment ou la passe
--   ecrit, et le nom seul se trompe d'homonyme. NUL = inconnu (credit ecrit
--   par une autre source : saisie, pont Roon, ancienne passe).
-- * `albums.credits_mb_at` — horodatage du dernier passage de la passe des
--   credits sur ce disque. C'est le CURSEUR de reprise : la passe ne reprend
--   que les disques ou il est NUL. NUL = jamais interroge.
--
-- Index : `artist_mbid` et `artist_name` sont les deux cles de la lecture de
-- la page artiste (`artist_id` a deja le sien depuis la 001). La 001 ne posait
-- pas celui du nom, que SQLite a depuis sa migration 9.
--
-- Idempotent : `IF NOT EXISTS` partout. Aucune reprise de donnees : rien dans
-- cette base ne portait ces deux informations.
BEGIN;
ALTER TABLE track_credits
    ADD COLUMN IF NOT EXISTS artist_mbid TEXT;
ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS credits_mb_at TEXT;
CREATE INDEX IF NOT EXISTS idx_track_credits_artist_mbid ON track_credits(artist_mbid);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist_name ON track_credits(artist_name);
INSERT INTO schema_version (version, name) VALUES (70, 'credits_musicbrainz')
    ON CONFLICT (version) DO NOTHING;
COMMIT;
