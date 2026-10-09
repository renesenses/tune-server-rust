-- 084_file_first_seen_created_at.sql
--
-- #5402 : la date de CRÉATION d'un fichier, gardée à côté de sa première vue.
-- Jumelle de la migration SQLite 120.
--
-- `file_first_seen.first_seen_at` reste la date d'ajout (modification
-- d'abord, #4546). `created_at` porte la date de création que le système de
-- fichiers donne (btime : `statx` sous Linux, `st_birthtime` sous macOS,
-- `ftCreationTime` sous Windows). NULL quand il ne la donne pas (NFS, SMB,
-- certains montages Docker) : le tri « par création » retombe alors sur la
-- date d'ajout. Aucun rattrapage : seuls les fichiers nouveaux ou rescannés
-- la reçoivent.
--
-- DOUBLE PRECISION comme `first_seen_at`.
--
-- La table n'est posée que par `ENSURE_TABLES` : sur une base neuve, elle
-- peut ne pas exister encore quand cette migration passe. D'où le CREATE
-- TABLE IF NOT EXISTS, identique à celui d'`ENSURE_TABLES`.
--
-- Idempotent : CREATE TABLE IF NOT EXISTS et ADD COLUMN IF NOT EXISTS sont
-- sûrs à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS file_first_seen (
    file_path TEXT PRIMARY KEY,
    first_seen_at DOUBLE PRECISION NOT NULL
);

ALTER TABLE file_first_seen
    ADD COLUMN IF NOT EXISTS created_at DOUBLE PRECISION;

INSERT INTO schema_version (version, name) VALUES (84, 'file_first_seen_created_at')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
