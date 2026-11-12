-- 114_spectrogrammes.sql
--
-- #8160 : le spectrogramme CONSERVÉ d'une piste, pour sa fiche technique.
-- Jumelle de la migration SQLite 148 (`migrations/spectrogrammes.sql`), qui
-- en donne le détail : l'image reste un PNG du cache plafonné des
-- spectrogrammes ; la table en garde la trace datée et la signature du
-- fichier audio (taille, date de modification) au moment du calcul.
--
-- La table naît VIDE et n'est pas recopiée à la bascule SQLite → PostgreSQL
-- (mesures recalculables, `TABLES_NON_COPIEES` de pg_migrate.rs).
--
-- Idempotent : CREATE TABLE / INDEX IF NOT EXISTS sont sûrs à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS spectrogrammes (
    track_id BIGINT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    fichier TEXT NOT NULL,
    largeur BIGINT NOT NULL,
    hauteur BIGINT NOT NULL,
    octets BIGINT NOT NULL,
    signature TEXT NOT NULL,
    taille_fichier BIGINT,
    mtime_fichier DOUBLE PRECISION,
    duree_calcul_ms BIGINT,
    calcule_le TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE INDEX IF NOT EXISTS idx_spectrogrammes_fichier ON spectrogrammes(fichier);

INSERT INTO schema_version (version, name) VALUES (114, 'spectrogrammes')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
