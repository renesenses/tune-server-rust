-- 121_index_noms_artistes_et_genres.sql
--
-- renesenses/tune-web-client#3118 (« Parce que vous aimez <artiste> ») :
-- la route compare les noms d'artistes sans tenir compte de la casse,
-- `LOWER(name) = LOWER($1)` sur PostgreSQL (SQLite passe par
-- `idx_artists_name`, en COLLATE NOCASE), et cherche les artistes d'un même
-- genre (`tracks.genre = $1`). Sans index, l'une et l'autre parcourent toute
-- la table. Pas de jumelle SQLite : `idx_artists_name` y existe déjà, et la
-- recherche par genre n'y tourne qu'une fois par période.
--
-- Idempotent : CREATE INDEX IF NOT EXISTS est sûr à rejouer.

BEGIN;

CREATE INDEX IF NOT EXISTS idx_artists_name_lower ON artists (LOWER(name));
CREATE INDEX IF NOT EXISTS idx_tracks_genre ON tracks (genre);

INSERT INTO schema_version (version, name) VALUES (121, 'index_noms_artistes_et_genres')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
