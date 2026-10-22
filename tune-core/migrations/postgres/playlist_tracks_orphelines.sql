-- #6179 — jumeau PostgreSQL de `migrations/playlist_tracks_orphelines.sql`
-- (SQLite) : AVANT la suppression d'une piste, chaque ligne de playlist qui la
-- designe perd son `track_id` (ligne INTROUVABLE, meme position) et recoit
-- l'identite de la piste qui part. Voir le fichier SQLite pour le pourquoi.
--
-- Rejoue a chaque demarrage par `run_pg_migrations`, APRES les scripts
-- numerotes (la 089 pose les colonnes qu'il ecrit). `CREATE OR REPLACE` et
-- `DROP TRIGGER IF EXISTS` : idempotent.

CREATE OR REPLACE FUNCTION playlist_tracks_garde_l_orpheline() RETURNS trigger AS $$
BEGIN
    UPDATE playlist_tracks SET
        track_id = NULL,
        missing_since = COALESCE(missing_since, EXTRACT(EPOCH FROM now())::BIGINT),
        title = COALESCE(NULLIF(OLD.title, ''), title),
        artist = COALESCE((SELECT name FROM artists WHERE id = OLD.artist_id), artist),
        album = COALESCE((SELECT title FROM albums WHERE id = OLD.album_id), album),
        duration_ms = COALESCE(NULLIF(OLD.duration_ms, 0), duration_ms),
        file_path = COALESCE(OLD.file_path, file_path),
        audio_hash = COALESCE(OLD.audio_hash, audio_hash),
        audio_fingerprint = COALESCE(OLD.audio_fingerprint, audio_fingerprint),
        mbid = COALESCE(OLD.musicbrainz_recording_id, mbid)
    WHERE track_id = OLD.id;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS playlist_tracks_garde_l_orpheline ON tracks;
CREATE TRIGGER playlist_tracks_garde_l_orpheline
    BEFORE DELETE ON tracks
    FOR EACH ROW EXECUTE FUNCTION playlist_tracks_garde_l_orpheline();
