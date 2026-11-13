-- 119_collection_physique_associations.sql
--
-- renesenses/tune-server-rust#8672 (decision de Bertrand du 13/11) : « Ma
-- collection physique », un disque « nulle part » associe A LA MAIN a un album
-- de la bibliotheque. Une ligne par release Discogs ; le lien prime sur le
-- rapprochement automatique et survit aux synchronisations Discogs (il n'est
-- pas dans la memoire du greffon). Jumelle de la migration SQLite 153
-- (118 = radio_fin_de_file_par_defaut, #8656, fusionnee avant).
--
-- Nait vide ; copiee a la bascule apres les scripts
-- (`MIGRATION_TABLES_APRES_SCRIPTS`). Idempotent.

BEGIN;

CREATE TABLE IF NOT EXISTS collection_physique_associations (
 release_id BIGINT NOT NULL PRIMARY KEY,
 album_id BIGINT NOT NULL,
 associe_le BIGINT NOT NULL
);

INSERT INTO schema_version (version, name) VALUES (119, 'collection_physique_associations')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
