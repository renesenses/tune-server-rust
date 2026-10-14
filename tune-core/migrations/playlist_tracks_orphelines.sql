-- #6179 — une piste qui quitte `tracks` ne sort plus des playlists.
--
-- Rejoue a CHAQUE demarrage par la passe finale de `run_migrations`
-- (`IF NOT EXISTS`), comme `upnp_catalog_revision.sql` : un declencheur tombe
-- (table recreee, base restauree) se repose tout seul. Jumeau PostgreSQL :
-- `postgres/playlist_tracks_orphelines.sql`.
--
-- AVANT la suppression d'une piste, chaque ligne de playlist qui la designe :
-- * perd son `track_id` — elle devient la troisieme forme du CHECK, la ligne
--   INTROUVABLE (ni piste, ni titre de service) — et garde sa `position` ;
-- * recoit l'identite de la piste, lue sur la ligne qui part : titre, artiste,
--   album et duree dans les colonnes d'affichage, chemin, empreintes et MBID
--   dans les siennes. C'est ce qui permet a `db/playlists_reconcile.rs` de la
--   rattacher a la piste quand elle reparait (meme fichier deplace, rescan,
--   disque rebranche), et a l'ecran de la nommer en attendant.
--
-- Un declencheur, et pas un appel dans chaque chemin de suppression : la
-- purge post-scan, le surveillant (fichier modifie = supprime puis reinsere),
-- `delete_by_path`, `delete_all`, la fusion de doublons, le retrait d'un
-- serveur UPnP… Un chemin oublie, c'etait une playlist videe sans trace —
-- exactement le defaut de #6179. Ici, aucun chemin ne peut l'oublier.
--
-- `COALESCE(nouveau, ancien)` : une valeur deja figee (ligne deja orpheline,
-- impossible en pratique) n'est jamais effacee par un NULL.
CREATE TRIGGER IF NOT EXISTS playlist_tracks_garde_l_orpheline
BEFORE DELETE ON tracks
BEGIN
    UPDATE playlist_tracks SET
        track_id = NULL,
        missing_since = COALESCE(missing_since, CAST(strftime('%s', 'now') AS INTEGER)),
        title = COALESCE(NULLIF(OLD.title, ''), title),
        artist = COALESCE((SELECT name FROM artists WHERE id = OLD.artist_id), artist),
        album = COALESCE((SELECT title FROM albums WHERE id = OLD.album_id), album),
        duration_ms = COALESCE(NULLIF(OLD.duration_ms, 0), duration_ms),
        file_path = COALESCE(OLD.file_path, file_path),
        audio_hash = COALESCE(OLD.audio_hash, audio_hash),
        audio_fingerprint = COALESCE(OLD.audio_fingerprint, audio_fingerprint),
        mbid = COALESCE(OLD.musicbrainz_recording_id, mbid)
    WHERE track_id = OLD.id;
END;
