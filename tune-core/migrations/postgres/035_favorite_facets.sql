-- favorite_facets : mettre en favori une VALEUR de facette (label, et demain
-- genre / format / annee) — #2442, FabienM fil 1557.
--
-- Pendant SQLite (migration 83). Pourquoi une table separee plutot qu'un
-- quatrieme `item_type` dans `favorites` : `favorites.item_id` est un entier
-- NOT NULL, et un label N'A PAS D'IDENTITE dans ce depot. Il n'existe ni table
-- `labels`, ni route bibliotheque : l'onglet Labels lit une FACETTE et
-- selectionne par CHAINE. Promouvoir le label en entite (normalisation d'un
-- champ libre, identifiants, jointures) est l'option couteuse, ecartee.
--
-- Pas de colonne `id` : la cle naturelle (profil, facette, valeur) EST la cle
-- primaire. Cela evite du meme coup la divergence BIGSERIAL / TEXT que la
-- bascule SQLite -> PostgreSQL impose a toute colonne `id` — la famille de
-- defauts que la migration 012 repare et qui a bricke le .15 (#1706).
--
-- `profile_id` en BIGINT comme `favorites.profile_id` (005), pour que les deux
-- tables se lisent et se joignent de la meme facon.
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS favorite_facets (
    profile_id BIGINT NOT NULL DEFAULT 1,
    facet TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, facet, value)
);

CREATE INDEX IF NOT EXISTS idx_favorite_facets_profile
    ON favorite_facets(profile_id, facet);

-- Rattrapage de la bascule SQLite -> PostgreSQL. `PG_FULL_SCHEMA` cree cette
-- table en TOUT TEXTE, parce que la copie de donnees lie chaque valeur SQLite
-- en texte et que PostgreSQL n'a pas de cast implicite texte -> entier a
-- l'INSERT. Le `CREATE TABLE IF NOT EXISTS` ci-dessus ne la corrige donc pas :
-- elle existe deja. C'est exactement la famille de defauts que la migration
-- 012 repare pour les autres tables copiees — sauf que 012 porte une liste
-- figee, ecrite avant cette table. On la repare donc ici, une fois.
--
-- Garde sur le type courant : sur une base ou la colonne est deja BIGINT,
-- l'ALTER serait au mieux inutile, au pire fatal (incident #1706).
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_name = 'favorite_facets'
           AND column_name = 'profile_id'
           AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE favorite_facets
            ALTER COLUMN profile_id DROP DEFAULT,
            ALTER COLUMN profile_id TYPE BIGINT
                USING NULLIF(TRIM(profile_id), '')::BIGINT;
        ALTER TABLE favorite_facets
            ALTER COLUMN profile_id SET DEFAULT 1;
    END IF;
END $$;
