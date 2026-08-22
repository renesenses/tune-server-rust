-- 031_colonnes_manquantes_pg.sql
--
-- Les trois colonnes du chantier « feuilles CUE » (#1763) n'existaient que dans
-- le schéma PostgreSQL NEUF, celui que `pg_migrate.rs` crée d'un bloc. Aucune
-- migration numérotée ne les ajoutait (#2111).
--
-- Or `CREATE TABLE` ne s'applique qu'à une base neuve. Une base PostgreSQL
-- montée par les scripts numérotés — c'est-à-dire toute base existante, .15 et
-- .18 comprises — n'a donc jamais reçu ces colonnes, et ne les aurait jamais
-- reçues.
--
-- Ce que ça n'a PAS cassé, et pourquoi personne ne s'en est aperçu : à ce jour
-- aucune requête ne nomme ces colonnes. Elles sont réservées, pas encore lues
-- (`grep -rn cue_media_path --include=*.rs` ne rend que les deux schémas). Le
-- défaut était donc entièrement latent — et il se serait réveillé à la première
-- requête du chantier CUE, sur les bases existantes uniquement, c'est-à-dire là
-- où on ne le teste pas.
--
-- Il s'est signalé de biais : la migration 030 lisait
-- `COALESCE(file_path, cue_media_path, '')` et le job qui applique les scripts
-- sur une base nue a refusé, avec raison — `column "cue_media_path" does not
-- exist`. 030 a été rendue conditionnelle pour ne pas bloquer #1612 ; le manque
-- lui-même est réparé ici.
--
-- Types alignés sur le CREATE TABLE de `pg_migrate.rs` (TEXT / BIGINT / BIGINT),
-- eux-mêmes alignés sur la migration SQLite 76 (TEXT / INTEGER / INTEGER).
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE tracks
    ADD COLUMN IF NOT EXISTS cue_media_path TEXT,
    ADD COLUMN IF NOT EXISTS cue_start_ms BIGINT,
    ADD COLUMN IF NOT EXISTS cue_end_ms BIGINT;

-- Le garde-fou écrit avec cette migration (`pg_schema_parity`) en a trouvé
-- SEPT autres dans le même cas, toutes sur `zones`. Ce n'était pas cherché :
-- le ticket ne parlait que des colonnes CUE.
--
-- Elles sont dans le schéma neuf, et `ENSURE_COLUMNS` — le rattrapage rejoué à
-- chaque démarrage — en soigne une vingtaine mais pas celles-ci. Une base
-- PostgreSQL existante ne les a donc pas, alors que six des sept commandent des
-- comportements de sortie DLNA bien réels (LPCM forcé, plafond 16 bits, FLAC
-- natif, passthrough ALAC/AAC, volume fixe).
--
-- Types repris tels quels du CREATE TABLE de `pg_migrate.rs` : TEXT 0/1 comme
-- tous les booléens copiés depuis SQLite. `mac` n'est pas dans ce CREATE TABLE,
-- seulement dans son bloc de rattrapage, où elle est TEXT.
ALTER TABLE zones
    ADD COLUMN IF NOT EXISTS mac TEXT,
    ADD COLUMN IF NOT EXISTS fixed_volume TEXT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS dlna_native_flac TEXT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS alac_passthrough TEXT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS aac_passthrough TEXT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS dlna_lpcm TEXT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS dlna_cap_16bit TEXT DEFAULT 0;

-- Et la dernière, isolée : `ENSURE_COLUMNS` soigne bien un `source_id`, mais
-- sur `listen_history`. Celui des abonnements aux podcasts (migration SQLite
-- v59) n'a jamais eu de jumelle PostgreSQL.
ALTER TABLE podcast_subscriptions
    ADD COLUMN IF NOT EXISTS source_id TEXT;

INSERT INTO schema_version (version, name) VALUES (31, 'colonnes_manquantes_pg')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
