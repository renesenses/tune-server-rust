-- #4907 — la même musique dans plusieurs répertoires. Jumelle de la
-- migration SQLite 110.
--
-- Numérotée 073, PAS 070 : les 070, 071 et 072 sont réservées par des PR
-- ouvertes en même temps que celle-ci (#4862, #4888, #4718/#4719). Un numéro
-- déjà appliqué sur une base ne se reprend jamais.
--
-- `track_copies` : les copies À L'IDENTIQUE d'une piste (même album, mêmes
-- octets). Aucune ligne `tracks` n'est touchée : les identifiants de pistes
-- que visent playlists, favoris, historique, notes et files d'attente ne
-- bougent pas. La table naît vide ; le prochain scan la remplit.
--
-- `album_preferred_roots` : le dossier de musique depuis lequel lire un album.
BEGIN;
CREATE TABLE IF NOT EXISTS track_copies (
    id BIGSERIAL PRIMARY KEY,
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    file_path TEXT NOT NULL UNIQUE,
    format TEXT,
    sample_rate INTEGER,
    bit_depth INTEGER,
    file_size BIGINT,
    file_mtime DOUBLE PRECISION,
    audio_hash TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE INDEX IF NOT EXISTS idx_track_copies_track ON track_copies(track_id);
CREATE TABLE IF NOT EXISTS album_preferred_roots (
    album_id BIGINT PRIMARY KEY REFERENCES albums(id) ON DELETE CASCADE,
    root TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
INSERT INTO schema_version (version, name) VALUES (73, 'exemplaires_par_repertoire') ON CONFLICT (version) DO NOTHING;
COMMIT;
