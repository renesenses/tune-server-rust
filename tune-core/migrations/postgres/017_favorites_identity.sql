-- 017_favorites_identity.sql
--
-- Instantané d'identité des favoris (SQLite migration v66) : les favoris
-- référencent des rowids d'albums/pistes/artistes, mais ces ids ne survivent
-- pas à un rescan qui recrée les items (racines music déplacées, library
-- clear, fusion de doublons) — cœurs éteints partout et filtre « Favoris »
-- vide (bug .18, v0.9.50). On fige titre/artiste/chemin à l'ajout du favori
-- pour que la réconciliation (db::favorites_reconcile, démarrage + post-scan)
-- puisse re-rattacher l'item vivant par identité au lieu de laisser des
-- favoris fantômes.
--
-- NULL sur les favoris existants ; backfillé à la première réconciliation.
--
-- Idempotent (ADD COLUMN IF NOT EXISTS).

BEGIN;

ALTER TABLE favorites ADD COLUMN IF NOT EXISTS item_name TEXT;
ALTER TABLE favorites ADD COLUMN IF NOT EXISTS item_artist TEXT;
ALTER TABLE favorites ADD COLUMN IF NOT EXISTS item_path TEXT;

INSERT INTO schema_version (version, name) VALUES (17, 'favorites_identity')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
