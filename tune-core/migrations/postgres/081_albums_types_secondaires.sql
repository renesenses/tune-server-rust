-- Les TYPES SECONDAIRES MusicBrainz du disque : live, compilation,
-- soundtrack, remix… separes par `;` (section « Live » de la fiche artiste,
-- decision de Bertrand du 05/10/2026).
--
-- Jumelle de la migration SQLite 117. Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posee d'un
-- seul cote ne repare que la moitie du parc (#1612, #2111). La fiche artiste
-- NOMME cette colonne (`AlbumRepo::types_secondaires_par_album`).
--
-- `albums.release_type` (migration 069) ne garde que le type PRIMAIRE : un
-- album live y reste un `album`. Sans cette colonne, un live restait dans la
-- section Albums.
--
-- Numerotee 81 : la 80 est prise par #5763 (`albums_identification_tentee_le`).
--
-- NUL = INCONNU. Remplie au scan depuis la balise `RELEASETYPE` (et
-- variantes), jamais par-dessus une valeur deja connue.
--
-- Idempotent : `IF NOT EXISTS`, sans danger a rejouer sur une base deja migree
-- depuis SQLite. Les lignes existantes gardent NULL.

BEGIN;

ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS release_secondary_types TEXT;

INSERT INTO schema_version (version, name) VALUES (81, 'albums_types_secondaires')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
