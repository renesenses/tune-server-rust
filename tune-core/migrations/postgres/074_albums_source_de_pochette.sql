-- D'OÙ VIENT la pochette d'un album (#5034, Didier, fil 1904).
--
-- Jumelle de la migration SQLite 111. Les deux listes sont SÉPARÉES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posée d'un
-- seul côté ne répare que la moitié du parc (#1612, #2111). Et celles-ci sont
-- NOMMÉES par le scan et le surveillant à chaque album relu.
--
-- `cover_source` : `embedded` (jaquette intégrée d'une piste), `folder`
-- (`cover.jpg`… du dossier), `upload` (téléversée à la main), `provider`
-- (Cover Art Archive, iTunes, Discogs, catalogue communautaire), `import`
-- (pont Roon). Seules les deux premières suivent le disque : leur fichier
-- disparu, la pochette est retirée ; leur fichier changé, elle change.
-- `cover_source_path` / `cover_source_stamp` : le fichier d'où l'image a été
-- tirée, et « mtime:taille » à ce moment-là.
--
-- 🔴 NUL = INCONNUE, pour TOUTES les lignes existantes : rien ne prouve
-- qu'une pochette déjà posée venait d'un fichier plutôt que d'un
-- téléversement (les deux sont adressées par le condensat du contenu). Une
-- source inconnue n'est jamais retirée.
--
-- Numérotée 074, PAS 073 : la 073 est prise par #4925 (exemplaires par
-- répertoire), PR ouverte en même temps. Le lanceur ne joue que
-- `version > MAX` : cette migration EXIGE la 073 avant elle, sinon elle se
-- renumérote à la promotion.
--
-- Idempotent : `IF NOT EXISTS`, sans danger à rejouer sur une base déjà
-- migrée depuis SQLite.

BEGIN;

ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS cover_source TEXT;
ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS cover_source_path TEXT;
ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS cover_source_stamp TEXT;

INSERT INTO schema_version (version, name) VALUES (74, 'albums_source_de_pochette')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
