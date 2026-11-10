-- 110_suggestions_ecartees.sql
--
-- renesenses/tune-web-client#916 (FabienM, fil forum 1527) : « Pas pour moi »
-- sur une suggestion de l'accueil, PAR PROFIL. Jumelle de la migration
-- SQLite 144 (numerotee 089 / SQLite 125 sur le chantier, renumerotee a
-- sa jonction avec batch/bugs-rc12-20261108 le 10/11).
--
-- Un album (local ou de service), une playlist de service, ou un artiste
-- designe par son nom normalise (`source = 'nom'`). Pas de colonne `id` : la
-- cle naturelle est la cle primaire, comme `hidden_items` et
-- `streaming_hidden_items`. `profile_id` en BIGINT : les filtres le comparent
-- a un entier.
--
-- Une base nee de la bascule SQLite -> PostgreSQL recoit la table par
-- `PG_FULL_SCHEMA` (avec sa copie) ; une base PG convertie avant cette version
-- la recoit par `ENSURE_TABLES`. Idempotent.

BEGIN;

CREATE TABLE IF NOT EXISTS suggestions_ecartees (
    profile_id BIGINT NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    source TEXT NOT NULL,
    item_id TEXT NOT NULL,
    title TEXT,
    subtitle TEXT,
    cover_url TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    PRIMARY KEY (profile_id, item_type, source, item_id)
);

INSERT INTO schema_version (version, name) VALUES (110, 'suggestions_ecartees')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
