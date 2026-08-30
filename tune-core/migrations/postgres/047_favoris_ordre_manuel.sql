-- #2001, piste 2 — l'ordre manuel des favoris.
--
-- Tades : « J'ai enregistre mes favoris dans le mauvais ordre et aurais aime
-- les ecouter dans l'ordre sequentiel. Ne parvenant pas a les deplacer par une
-- manoeuvre de souris… » La piste 1 de l'issue — le tri par champ — est arrivee
-- avec la PR #2829 (`favorites_sort`). Elle range d'apres un champ ; elle ne
-- rend PAS le geste qu'il avait tente, qui est de DEPLACER un favori. Il faut
-- pour cela un rang ecrit par l'utilisateur, donc une colonne.
--
-- Jumelle PostgreSQL de la migration SQLite 95. Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posee d'un seul
-- cote ne rejoindrait JAMAIS une base PostgreSQL deja creee (.15, .18, Docker),
-- et le defaut resterait invisible jusqu'au jour ou une requete la nommerait
-- (#2111, #1612). Les requetes de tri la nomment des cette version.
--
-- NULLABLE, et NULL veut dire « jamais range a la main » : c'est l'etat de
-- toutes les lignes existantes, et le tri manuel les met en FIN de liste
-- (regle 2 de `favorites_sort`). Aucun DEFAULT : un `DEFAULT 0` donnerait a
-- tout le parc le meme rang — un ordre manuel qui ne range rien tout en
-- pretendant exister.
--
-- TEXT et non BIGINT : ce schema PostgreSQL porte deja `profile_id`, `item_id`
-- et `id` en TEXT sur ces deux tables, parce que la copie SQLite -> PostgreSQL
-- lie chaque valeur en texte et que PostgreSQL n'a pas de cast implicite
-- texte -> entier a l'INSERT. Une colonne d'un autre type y serait la seule
-- exception, et lier un i64 y rendrait « column is of type text but expression
-- is of type bigint » — le 500 que `profile_repo::add_favorite` documente deja.
-- `SqlValue::as_i64` reconvertit le TEXT a la relecture.
--
-- Et le rang est compare EN RUST, jamais par un `ORDER BY position` : sur cette
-- colonne TEXT, PostgreSQL rangerait « 10 » avant « 2 », et SQLite (affinite
-- INTEGER) ferait l'inverse — deux ordres differents pour la meme bibliotheque.
--
-- Les deux tables que Tune POSSEDE la recoivent. Les favoris lus en direct chez
-- Qobuz/Tidal n'ont pas de ligne ici : ils reviennent du service a chaque
-- resynchronisation, et leur donner un rang durable demanderait une table de
-- correspondance — arbitrage non rendu.
--
-- Idempotent : `IF NOT EXISTS`, sans danger a rejouer sur une base deja migree
-- depuis SQLite ou la colonne est deja la.
ALTER TABLE favorites
    ADD COLUMN IF NOT EXISTS position TEXT;

-- `streaming_favorites` n'est PAS garantie ici : elle nait de `PG_FULL_SCHEMA`
-- (base neuve ou bascule) ou de `ENSURE_TABLES` (postgres.rs), pas d'une
-- migration numerotee. Un `ALTER TABLE` nu ferait donc echouer TOUTE la
-- migration 047 — donc le demarrage — sur une base ou elle n'existe pas encore.
-- Le garde `to_regclass` rend NULL au lieu de lever, contrairement a un
-- `ALTER TABLE IF EXISTS`, qui n'avertit que par un NOTICE et laisserait la
-- colonne manquante en silence de l'autre cote.
DO $ordre_manuel$
BEGIN
    IF to_regclass('streaming_favorites') IS NOT NULL THEN
        ALTER TABLE streaming_favorites ADD COLUMN IF NOT EXISTS position TEXT;
    END IF;
END $ordre_manuel$;
