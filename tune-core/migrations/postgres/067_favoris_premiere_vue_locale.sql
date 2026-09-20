-- 067_favoris_premiere_vue_locale.sql
--
-- renesenses/tune-web-client#1060 : la date que TUNE pose lui-même sur un
-- favori de service, à la première fois qu'il le voit. Jumelle de la
-- migration SQLite 104.
--
-- POURQUOI. `created_at` porte la date du SERVICE. Elle lui appartient, et il
-- la refait : mesuré le 19/09/2026 sur le serveur de Bertrand, vingt et un
-- favoris Qobuz portant vingt et une dates distinctes réparties sur SEIZE
-- SECONDES (2026-09-16T08:11:50Z … 08:12:06Z). Ce n'est pas l'histoire d'un
-- auditeur, c'est l'instant où une recopie les a recréés chez Qobuz. Le tri
-- « Ajout récent » rend alors l'ordre d'une boucle.
--
-- `first_seen_at` est posé à l'insertion et n'est JAMAIS réécrit : la requête
-- de redatation ne le nomme pas, et le rattrapage porte `WHERE first_seen_at
-- IS NULL`. Même leçon que `media_servers.first_seen_at` (PG 058) et
-- `zones.last_seen_at` (PG 050).
--
-- RÈGLE DE MIGRATION, écrite et non devinée : les lignes déjà en base
-- reçoivent `first_seen_at = created_at`. C'est la seule date dont on dispose
-- ici ; elle est fausse pour les favoris que le service a redatés, mais elle
-- est MESURÉE, pas fabriquée, et elle ne peut pas être pire que ce que l'écran
-- lit aujourd'hui. Une ligne sans `created_at` reste à NULL, et le client
-- retombe alors sur la date du service — exactement comme avant. Rien n'est
-- inventé en silence.
--
-- TEXT comme côté SQLite, comme `created_at` et comme `zones.last_seen_at` :
-- rien à rattraper dans la parité de types.
--
-- Idempotent : `ADD COLUMN IF NOT EXISTS` est sûr à rejouer, et l'UPDATE ne
-- touche QUE les lignes encore nulles — le rejouer ne peut pas réécrire une
-- date déjà posée.

BEGIN;

-- `streaming_favorites` n'est PAS garantie ici : elle naît de `PG_FULL_SCHEMA`
-- (base neuve ou bascule) ou de `ENSURE_TABLES` (postgres.rs), jamais d'une
-- migration numérotée. Un `ALTER TABLE` nu fait donc échouer TOUTE cette
-- migration — donc le démarrage — sur une base où elle n'existe pas encore.
-- Mesuré : la porte « Test (PostgreSQL) » de cette PR est partie ROUGE sur
-- `ERROR: relation "streaming_favorites" does not exist`, le lanceur jouant les
-- scripts numérotés SEULS sur une base vierge.
--
-- Le garde `to_regclass` rend NULL au lieu de lever, contrairement à un
-- `ALTER TABLE IF EXISTS` qui n'avertit que par un NOTICE et laisserait la
-- colonne manquante en silence. Même forme, mot pour mot, que la migration 057
-- (`favoris_ordre_manuel`), qui a payé exactement ce prix pour `position`.
DO $premiere_vue$
BEGIN
    IF to_regclass('streaming_favorites') IS NOT NULL THEN
        ALTER TABLE streaming_favorites ADD COLUMN IF NOT EXISTS first_seen_at TEXT;

        UPDATE streaming_favorites
           SET first_seen_at = created_at
         WHERE first_seen_at IS NULL
           AND created_at IS NOT NULL;
    ELSE
        RAISE NOTICE 'migration 067 : streaming_favorites absente, rien a dater';
    END IF;
END $premiere_vue$;

INSERT INTO schema_version (version, name) VALUES (67, 'favoris_premiere_vue_locale')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
