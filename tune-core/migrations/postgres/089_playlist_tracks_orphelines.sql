-- 089_playlist_tracks_orphelines.sql
--
-- #6179 (FabienM, fil 2236) : une piste supprimee ou re-identifiee par un
-- scan ne sort plus EN SILENCE des playlists locales. Jumelle de la migration
-- SQLite 125.
--
-- Une ligne de `playlist_tracks` prend une troisieme forme, la ligne
-- INTROUVABLE : ni `track_id`, ni `source` / `source_id`. Elle garde sa
-- position et l'identite de la piste disparue, que le declencheur
-- `playlist_tracks_garde_l_orpheline` (`playlist_tracks_orphelines.sql`,
-- rejoue a chaque demarrage par `run_pg_migrations`) y fige AVANT la
-- suppression. La reconciliation (`db/playlists_reconcile.rs`) la rattache a
-- la piste quand elle reparait.
--
-- Les DEUX naissances d'une base PostgreSQL passent ici :
--
-- * installation native : `track_id` porte `REFERENCES tracks(id) ON DELETE
--   CASCADE` (001). La clef etrangere est RECREEE en `ON DELETE SET NULL` :
--   meme sans le declencheur, une suppression de piste n'efface plus la ligne.
-- * bascule depuis SQLite : la table vient de `PG_FULL_SCHEMA`, tout en TEXT,
--   SANS clef etrangere, et les colonnes nouvelles y sont DEJA. Rien a
--   recreer cote clef ; `missing_since` est CONVERTIE en BIGINT (meme garde
--   que la 088).
--
-- Le CHECK `playlist_tracks_piste_ou_titre_de_service` (072) est remplace par
-- `playlist_tracks_piste_titre_ou_introuvable`, qui admet la troisieme forme
-- et refuse toujours une ligne a la fois locale et de service.
--
-- Idempotent : rejouable sans danger (sentinelle 99 de la bascule).

BEGIN;

ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS file_path TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS audio_hash TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS audio_fingerprint TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS mbid TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS missing_since BIGINT;

DO $orphelines_type$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'playlist_tracks'
       AND column_name = 'missing_since';
    IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
        ALTER TABLE playlist_tracks ALTER COLUMN missing_since TYPE BIGINT USING (
            CASE WHEN missing_since::text ~ '^[0-9]+$' THEN missing_since::text::bigint END
        );
        RAISE NOTICE 'migration 089 : playlist_tracks.missing_since % -> bigint', typ;
    END IF;
END $orphelines_type$;

DO $orphelines_check$
BEGIN
    IF EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'playlist_tracks_piste_ou_titre_de_service'
           AND conrelid = 'playlist_tracks'::regclass
    ) THEN
        ALTER TABLE playlist_tracks
            DROP CONSTRAINT playlist_tracks_piste_ou_titre_de_service;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'playlist_tracks_piste_titre_ou_introuvable'
           AND conrelid = 'playlist_tracks'::regclass
    ) THEN
        ALTER TABLE playlist_tracks
            ADD CONSTRAINT playlist_tracks_piste_titre_ou_introuvable CHECK (
                (track_id IS NOT NULL AND source IS NULL AND source_id IS NULL)
                OR (track_id IS NULL AND source IS NOT NULL AND source_id IS NOT NULL)
                OR (track_id IS NULL AND source IS NULL AND source_id IS NULL)
            );
    END IF;
END $orphelines_check$;

-- La clef etrangere `track_id → tracks(id)`, si elle existe et CASCADE,
-- devient SET NULL. Cherchee par sa definition et non par son nom : 001 la
-- laisse nommer par PostgreSQL.
DO $orphelines_fk$
DECLARE
    nom TEXT;
BEGIN
    FOR nom IN
        SELECT c.conname FROM pg_constraint c
         WHERE c.conrelid = 'playlist_tracks'::regclass
           AND c.confrelid = 'tracks'::regclass
           AND c.contype = 'f'
           AND c.confdeltype = 'c'
    LOOP
        EXECUTE format('ALTER TABLE playlist_tracks DROP CONSTRAINT %I', nom);
        ALTER TABLE playlist_tracks
            ADD CONSTRAINT playlist_tracks_track_id_fkey
            FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE SET NULL;
        RAISE NOTICE 'migration 089 : % -> ON DELETE SET NULL', nom;
    END LOOP;
END $orphelines_fk$;

INSERT INTO schema_version (version, name) VALUES (89, 'playlist_tracks_orphelines')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
