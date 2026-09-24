-- #4889 — une playlist Tune peut porter un TITRE DE SERVICE (Bandcamp,
-- Qobuz, Tidal…), pas seulement une piste de la bibliotheque (FabienM, fil
-- 1906, reponse 6704).
--
-- Jumelle de la migration SQLite 109. Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc un changement pose d'un
-- seul cote ne repare que la moitie du parc (#1612, #2111).
--
-- Une ligne de `playlist_tracks` est SOIT une piste locale (`track_id`), SOIT
-- une paire `source` / `source_id`, avec de quoi l'afficher sans appeler le
-- service a chaque liste : titre, artiste, album, album chez le service,
-- duree, pochette — la forme de `queue_items`. Le CHECK
-- `playlist_tracks_piste_ou_titre_de_service` garantit « exactement l'un des
-- deux ».
--
-- Les DEUX naissances d'une base PostgreSQL passent ici :
--
-- * installation native : la table vient de 001 (`track_id BIGINT NOT NULL
--   REFERENCES tracks(id)`). Le NOT NULL tombe ; la clef etrangere RESTE —
--   une ligne locale designe toujours une piste qui existe, et une ligne de
--   service (track_id NUL) n'est pas concernee par elle.
-- * bascule depuis SQLite : la table vient de `PG_FULL_SCHEMA`, tout en TEXT,
--   et les colonnes nouvelles y sont DEJA (la copie des donnees les
--   alimente). `ADD COLUMN IF NOT EXISTS` n'y fait rien : `duration_ms` est
--   donc CONVERTIE en BIGINT, comme la 071 le fait pour ses colonnes.
--
-- Aucune clef d'unicite : une playlist a toujours pu porter deux fois la meme
-- piste (fusion sans dedoublonnage, playlist de dossier). Le dedoublonnage
-- reste le travail du chemin d'ajout.
--
-- Idempotent : rejouable sans danger (sentinelle 99 de la bascule). Les deux
-- gardes lisent le schema COURANT : `information_schema.columns` sans filtre
-- de schema a deja converti la mauvaise table (038, integration v0.9.164).

BEGIN;

ALTER TABLE playlist_tracks
    ALTER COLUMN track_id DROP NOT NULL;

ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS source TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS source_id TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS title TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS artist TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS album TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS album_source_id TEXT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS duration_ms BIGINT;
ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS cover_url TEXT;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'playlist_tracks'
           AND column_name = 'duration_ms'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE playlist_tracks ALTER COLUMN duration_ms DROP DEFAULT;
        ALTER TABLE playlist_tracks
            ALTER COLUMN duration_ms TYPE BIGINT
            USING NULLIF(TRIM(duration_ms), '')::BIGINT;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'playlist_tracks_piste_ou_titre_de_service'
           AND conrelid = 'playlist_tracks'::regclass
    ) THEN
        ALTER TABLE playlist_tracks
            ADD CONSTRAINT playlist_tracks_piste_ou_titre_de_service CHECK (
                (track_id IS NOT NULL AND source IS NULL AND source_id IS NULL)
                OR (track_id IS NULL AND source IS NOT NULL AND source_id IS NOT NULL)
            );
    END IF;
END $$;

INSERT INTO schema_version (version, name) VALUES (72, 'playlist_tracks_titres_de_service')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
