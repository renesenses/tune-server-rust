-- hidden_items : masquer un album de la bibliothèque sans toucher aux
-- fichiers (#1391, Jean-Luc Cassé).
--
-- Jumelle PostgreSQL de la migration SQLite 89. Les deux listes sont
-- SEPAREES : écrite d'un seul côté, la table manquerait à tout le parc
-- PostgreSQL (.15, .18, Docker) et chaque vue bibliothèque y rendrait une
-- erreur SQL, puisque le filtre « pas masqué » la nomme.
--
-- Pourquoi une table et pas `albums.is_hidden` : une ligne `albums` est
-- supprimée en routine (purge post-scan, delete_orphans, fusion de doublons,
-- « vider la bibliothèque ») — le drapeau mourrait avec elle, exactement le
-- défaut déjà payé par `favorites` (cœurs éteints, bug .18 v0.9.50). On
-- reprend la solution des favoris : marqueur sans clé étrangère, instantané
-- d'identité (`item_name`/`item_artist`), réconciliation au démarrage et
-- post-scan (`hidden_repo::reconcile`).
--
-- PAS DE COLONNE `id` : la clé naturelle (profil, type, item) EST la clé
-- primaire — même choix que `favorite_facets` (PG 038) et `task_runs`
-- (PG 040), pour éviter la divergence AUTOINCREMENT / BIGSERIAL que la
-- bascule SQLite -> PostgreSQL impose à toute colonne `id` (#1706).
--
-- `profile_id` est ÉCRIT (toujours 1) mais jamais LU par les filtres :
-- masquage global aujourd'hui, par-profil possible demain sans migration.
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS hidden_items (
    profile_id BIGINT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id BIGINT NOT NULL,
    item_name TEXT,
    item_artist TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, item_type, item_id)
);

CREATE INDEX IF NOT EXISTS idx_hidden_items_item ON hidden_items(item_type, item_id);

-- Rattrapage de la bascule SQLite -> PostgreSQL. `PG_FULL_SCHEMA` crée cette
-- table en TOUT TEXTE, parce que la copie de données lie chaque valeur SQLite
-- en texte et que PostgreSQL n'a pas de cast implicite texte -> entier à
-- l'INSERT. Le `CREATE TABLE IF NOT EXISTS` ci-dessus ne la corrige donc
-- pas : elle existe déjà. Même réparation, colonne par colonne, que
-- `favorite_facets` (PG 038).
--
-- Garde sur le type courant : sur une base où la colonne est déjà BIGINT,
-- l'ALTER serait au mieux inutile, au pire fatal (incident #1706).
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'hidden_items'
           AND column_name = 'profile_id'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE hidden_items
            ALTER COLUMN profile_id DROP DEFAULT,
            ALTER COLUMN profile_id TYPE BIGINT
                USING NULLIF(TRIM(profile_id), '')::BIGINT;
        ALTER TABLE hidden_items
            ALTER COLUMN profile_id SET DEFAULT 1;
    END IF;
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'hidden_items'
           AND column_name = 'item_id'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE hidden_items
            ALTER COLUMN item_id TYPE BIGINT
                USING NULLIF(TRIM(item_id), '')::BIGINT;
    END IF;
END $$;
