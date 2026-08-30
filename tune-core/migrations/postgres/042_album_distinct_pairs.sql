-- album_distinct_pairs : « ces deux albums ne sont PAS des doublons »
-- (#1276, Megalo — forum-hifi.fr #41831 p.13).
--
-- Jumelle PostgreSQL de la migration SQLite 90. Les deux listes sont
-- SEPAREES : écrite d'un seul côté, la table manquerait à tout le parc
-- PostgreSQL (.15, .18, Docker) et les deux routes qui la nomment
-- (`/library/albums/grouped`, `/library/albums/merge-duplicates`) y
-- rendraient une erreur SQL.
--
-- Pourquoi une table de PAIRES sans clé étrangère : une ligne `albums` est
-- supprimée en routine (purge post-scan, delete_orphans, fusion de doublons,
-- « vider la bibliothèque ») — un couple d'ids nu mourrait au premier
-- déplacement de racine, et l'arbitrage de l'utilisateur avec. Même défaut
-- que `favorites` (#1248), même solution que `hidden_items` (#1391) :
-- instantané d'identité figé des DEUX côtés, réconciliation au démarrage,
-- post-scan et purge (`album_distinct_repo::reconcile`), par le MÊME
-- `find_album_by_identity`.
--
-- La paire est rangée en (min, max) par le repo : « A n'est pas un doublon de
-- B » ne dépend pas de l'ordre.
--
-- PAS DE COLONNE `id` : la clé naturelle (profil, album bas, album haut) EST
-- la clé primaire — même choix que `favorite_facets` (PG 038), `task_runs`
-- (PG 040) et `hidden_items` (PG 041), pour éviter la divergence
-- AUTOINCREMENT / BIGSERIAL que la bascule SQLite -> PostgreSQL impose à
-- toute colonne `id` (#1706).
--
-- `profile_id` est ÉCRIT (toujours 1) mais jamais LU : arbitrage global
-- aujourd'hui, par-profil possible demain sans migration.
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS album_distinct_pairs (
    profile_id BIGINT NOT NULL DEFAULT 1,
    album_a_id BIGINT NOT NULL,
    album_b_id BIGINT NOT NULL,
    a_name TEXT,
    a_artist TEXT,
    b_name TEXT,
    b_artist TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, album_a_id, album_b_id)
);

CREATE INDEX IF NOT EXISTS idx_album_distinct_pairs_b ON album_distinct_pairs(album_b_id);

-- Rattrapage de la bascule SQLite -> PostgreSQL. `PG_FULL_SCHEMA` crée cette
-- table en TOUT TEXTE, parce que la copie de données lie chaque valeur SQLite
-- en texte et que PostgreSQL n'a pas de cast implicite texte -> entier à
-- l'INSERT. Le `CREATE TABLE IF NOT EXISTS` ci-dessus ne la corrige donc pas :
-- elle existe déjà. Même réparation, colonne par colonne, que `hidden_items`
-- (PG 041) et `favorite_facets` (PG 038).
--
-- Garde sur le type courant : sur une base où la colonne est déjà BIGINT,
-- l'ALTER serait au mieux inutile, au pire fatal (incident #1706).
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'album_distinct_pairs'
           AND column_name = 'profile_id'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE album_distinct_pairs
            ALTER COLUMN profile_id DROP DEFAULT,
            ALTER COLUMN profile_id TYPE BIGINT
                USING NULLIF(TRIM(profile_id), '')::BIGINT;
        ALTER TABLE album_distinct_pairs
            ALTER COLUMN profile_id SET DEFAULT 1;
    END IF;
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'album_distinct_pairs'
           AND column_name = 'album_a_id'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE album_distinct_pairs
            ALTER COLUMN album_a_id TYPE BIGINT
                USING NULLIF(TRIM(album_a_id), '')::BIGINT;
    END IF;
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'album_distinct_pairs'
           AND column_name = 'album_b_id'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE album_distinct_pairs
            ALTER COLUMN album_b_id TYPE BIGINT
                USING NULLIF(TRIM(album_b_id), '')::BIGINT;
    END IF;
END $$;
