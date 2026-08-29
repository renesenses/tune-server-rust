-- #2111 : les trois colonnes CUE et leur index d'identité, côté PostgreSQL.
--
-- La migration SQLite 76 (#1763) a posé `cue_media_path`, `cue_start_ms`,
-- `cue_end_ms` et l'index unique qui donne une identité aux pistes virtuelles.
-- PostgreSQL n'a reçu **que la déclaration du schéma neuf** (`pg_migrate.rs`) :
-- aucune migration numérotée ne les ajoute à une table `tracks` déjà créée.
--
-- Conséquence : une base PostgreSQL antérieure au chantier CUE ne les a jamais
-- reçues, et ne les recevra jamais — `CREATE TABLE` ne s'applique qu'à une base
-- vide. Le job `test-postgres`, qui applique ces fichiers sur une base nue, l'a
-- révélé en refusant une migration qui nommait `cue_media_path`.
--
-- ⚠️ CE N'EST PAS URGENT, ET C'EST EXACTEMENT POUR ÇA QU'IL FAUT LE POSER
-- MAINTENANT.
--
-- Rien ne lit ni n'écrit ces colonnes aujourd'hui : `scanner/cue.rs` ne fait
-- que lire du texte — « il ne touche ni au disque, ni à la base, ni au
-- décodeur », dit son en-tête — et personne ne l'appelle. Le socle a été posé
-- avant ses consommateurs, délibérément.
--
-- Le jour où le lot suivant branchera l'écriture, une base PostgreSQL ancienne
-- échouera. Et l'échec se lira comme un défaut du CUE, pas comme une migration
-- manquante — on cherchera dans le mauvais fichier.
--
-- L'index est le point qui compte le plus. Les pistes virtuelles partagent
-- toutes UN fichier, donc `file_path` reste NUL pour elles et leur identité
-- vient du couple `(cue_media_path, cue_start_ms)`. Sans cet index, chaque
-- scan les recréerait, faute de pouvoir les retrouver. Il est partiel — sur les
-- seules lignes CUE — pour ne rien imposer aux pistes ordinaires, dont
-- `cue_media_path` est NUL et le resterait par milliers.

ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cue_media_path TEXT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cue_start_ms   BIGINT;
ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cue_end_ms     BIGINT;

-- `BIGINT` et non `INTEGER` : c'est le type que `pg_migrate.rs` déclare pour
-- ces deux colonnes dans le schéma neuf. Les faire diverger d'une base à
-- l'autre reproduirait le défaut que cette migration corrige, un cran plus bas.

CREATE UNIQUE INDEX IF NOT EXISTS idx_tracks_cue_identity
    ON tracks (cue_media_path, cue_start_ms)
 WHERE cue_media_path IS NOT NULL;
