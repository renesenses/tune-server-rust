-- 067_albums_compilation_manuelle.sql
--
-- La décision MANUELLE sur le drapeau « compilation » (#4427).
--
-- `is_compilation` porte ce que le SCAN a déduit. Cette colonne porte ce que
-- l'UTILISATEUR a voulu, et les deux sont des informations différentes :
-- écraser l'une par l'autre rendrait le retour en arrière impossible, et le
-- prochain scan défairait le geste.
--
-- NULL = personne n'a tranché (l'état de toutes les lignes existantes) : le
-- scan décide comme avant. 0/1 = choix explicite, que ni `mark_compilation`
-- ni la passe de réparation ne touchent — même règle que l'enrichissement,
-- où ce qui a été posé à la main est protégé.
--
-- Bertrand, 18/09/2026 : « Coco María Presents » éclaté en douze vignettes,
-- une par artiste de piste. Il veut poser le drapeau à la main, et que ce
-- choix tienne.
--
-- SMALLINT et non BOOLEAN : la convention des booléens de ce schéma, avec la
-- nuance que NULL y est porteur de sens. Idempotent.

BEGIN;

ALTER TABLE albums ADD COLUMN IF NOT EXISTS compilation_manuelle SMALLINT;

INSERT INTO schema_version (version, name) VALUES (67, 'albums_compilation_manuelle')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
