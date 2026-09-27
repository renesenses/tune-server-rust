-- #5192 — les termes de chemin d'une piste (nom du dernier dossier et nom du
-- fichier sans extension, `_ - .` en espaces), en colonne CALCULÉE et STOCKÉE.
--
-- Le texte libre d'Oxygen (`facet_filter::condition_texte_libre`) les lisait
-- en recalculant l'expression à chaque ligne : 4 s pour 100 000 pistes. La
-- colonne est calculée à l'écriture, une fois.
--
-- L'expression est celle de `full_text_search::sql_termes_de_chemin`, appliquée
-- à `COALESCE(file_path, cue_media_path)`, MOT POUR MOT : l'épreuve
-- `la_migration_pg_076_porte_l_expression_des_termes_de_chemin` compare ce
-- fichier au constructeur Rust, et `pg_5192` compare la colonne à
-- `termes_de_chemin` sur un corpus de chemins. Elle n'emploie que des fonctions
-- IMMUTABLE (REPLACE, RTRIM, SUBSTR, LENGTH, TRIM, COALESCE, LIKE), condition
-- d'une colonne générée.
--
-- Pas d'index : un `LIKE '%…%'` sur `LOWER(unaccent(…))` n'en utilise aucun
-- (unaccent est STABLE, pas IMMUTABLE, et pg_trgm n'est pas garanti).
--
-- `search_tsv`, lui, reste calculé par `tracks_search_tsv_refresh` : une
-- colonne générée n'est pas encore connue d'un déclencheur BEFORE.

BEGIN;

ALTER TABLE tracks ADD COLUMN IF NOT EXISTS path_terms TEXT
    GENERATED ALWAYS AS (
(CASE WHEN REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/') LIKE '%://%' THEN '' ELSE TRIM(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(SUBSTR(RTRIM(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', '')), '/'), LENGTH(RTRIM(RTRIM(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', '')), '/'), REPLACE(RTRIM(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', '')), '/'), '/', ''))) + 1) || ' ' || (CASE WHEN RTRIM(SUBSTR(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), LENGTH(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', ''))) + 1), REPLACE(SUBSTR(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), LENGTH(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', ''))) + 1), '.', '')) = '' THEN SUBSTR(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), LENGTH(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', ''))) + 1) ELSE SUBSTR(SUBSTR(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), LENGTH(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', ''))) + 1), 1, LENGTH(RTRIM(SUBSTR(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), LENGTH(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', ''))) + 1), REPLACE(SUBSTR(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), LENGTH(RTRIM(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), REPLACE(REPLACE(COALESCE(COALESCE(file_path, cue_media_path), ''), '\', '/'), '/', ''))) + 1), '.', ''))) - 1) END), '_', ' '), '-', ' '), '.', ' '), '  ', ' '), '  ', ' '), '  ', ' ')) END)
    ) STORED;

INSERT INTO schema_version (version, name) VALUES (76, 'tracks_path_terms')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
