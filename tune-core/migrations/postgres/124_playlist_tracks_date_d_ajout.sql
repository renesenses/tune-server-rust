-- #9362 (fil forum 3000, accord de Bertrand du 22/11/2026, rc15) — la date
-- d'ajout d'une ligne A SA PLAYLIST, en secondes depuis l'epoque Unix (meme
-- unite que `missing_since`). Jumelle de la SQLite 158.
--
-- NULL pour l'existant : aucune base ne gardait cette date. Pas de DEFAULT :
-- comme cote SQLite, c'est `PlaylistRepo` qui la pose (l'heure de l'ajout
-- pour une ligne neuve, la date d'avant pour une ligne recopiee par une
-- reecriture de l'ordre).
--
-- Les DEUX naissances d'une base PostgreSQL passent ici :
--
-- * installation native : la colonne naît BIGINT ;
-- * bascule depuis SQLite : `PG_FULL_SCHEMA` la cree en TEXT (la copie lie
--   chaque valeur en texte) ; `ADD COLUMN IF NOT EXISTS` n'y fait rien, elle
--   est donc CONVERTIE, comme la 089 pour `missing_since`.
--
-- Idempotent : rejouable sans danger. La garde lit le schema COURANT.

BEGIN;

ALTER TABLE playlist_tracks ADD COLUMN IF NOT EXISTS added_at BIGINT;

DO $date_d_ajout_type$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'playlist_tracks'
       AND column_name = 'added_at';
    IF typ IN ('text', 'character varying', 'integer', 'smallint') THEN
        ALTER TABLE playlist_tracks ALTER COLUMN added_at TYPE BIGINT USING (
            CASE WHEN added_at::text ~ '^[0-9]+$' THEN added_at::text::bigint END
        );
        RAISE NOTICE 'migration 124 : playlist_tracks.added_at % -> bigint', typ;
    END IF;
END $date_d_ajout_type$;

INSERT INTO schema_version (version, name) VALUES (124, 'playlist_tracks_date_d_ajout')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
