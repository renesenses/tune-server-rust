-- 078_references_d_album_de_service.sql
--
-- Fil 2121 (FabienM, 03/10) : la RÉFÉRENCE D'ALBUM d'une piste de service
-- (`StreamTrack.album_id`), gardée avec la piste dans la file, les favoris de
-- service et l'historique. Jumelle de la migration SQLite 114.
--
-- Pour Bandcamp, c'est l'adresse de la page album ou piste : la seule chose
-- qui permette de resigner une URL de flux bcbits expirée (410 au bout de
-- quelques jours). Le `source_id` d'une piste Bandcamp EST l'URL signée, et
-- aucune des trois tables ne gardait la page.
--
-- NULL pour les lignes existantes : rien ne dit de quelle page venait une
-- piste déjà rangée. TEXT comme côté SQLite : rien à rattraper dans la parité
-- de types.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

-- Aucune des trois tables n'est garantie ici : `streaming_favorites` (et
-- selon le chemin `queue_items`) naît de `PG_FULL_SCHEMA` ou de
-- `ENSURE_TABLES`, jamais d'une migration numérotée. Un `ALTER TABLE` nu fait
-- échouer toute la migration, donc le démarrage, sur une base où la table
-- n'existe pas encore. Mesuré sur cette PR : la parité PG, qui rejoue les
-- scripts numérotés SEULS sur une base vierge, est partie rouge sur
-- `relation "streaming_favorites" does not exist`. Même garde `to_regclass`
-- que les migrations 057 et 067 ; la colonne est alors posée par
-- `ENSURE_COLUMNS` (postgres.rs) et par `PG_FULL_SCHEMA` (pg_migrate.rs).
DO $references_d_album$
BEGIN
    IF to_regclass('queue_items') IS NOT NULL THEN
        ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS album_ref TEXT;
    ELSE
        RAISE NOTICE 'migration 078 : queue_items absente';
    END IF;
    IF to_regclass('streaming_favorites') IS NOT NULL THEN
        ALTER TABLE streaming_favorites ADD COLUMN IF NOT EXISTS album_ref TEXT;
    ELSE
        RAISE NOTICE 'migration 078 : streaming_favorites absente';
    END IF;
    IF to_regclass('listen_history') IS NOT NULL THEN
        ALTER TABLE listen_history ADD COLUMN IF NOT EXISTS album_ref TEXT;
    ELSE
        RAISE NOTICE 'migration 078 : listen_history absente';
    END IF;
END $references_d_album$;

INSERT INTO schema_version (version, name) VALUES (78, 'references_d_album_de_service')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
