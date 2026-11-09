-- Migration SQLite 143 (#8160) : le spectrogramme CONSERVÉ d'une piste, pour
-- sa fiche technique.
--
-- L'image elle-même reste un PNG du cache des spectrogrammes
-- (`<cache des pochettes>/spectrogrammes/`, plafonné, éviction des images
-- servies le moins récemment, #7857). Cette table en garde la trace datée :
-- le nom du fichier, sa taille, sa largeur et sa hauteur, la date et la durée
-- du calcul, et la SIGNATURE du fichier audio au moment du calcul (taille et
-- date de modification), qui dit si l'image vaut encore pour le fichier
-- d'aujourd'hui.
--
-- Une ligne par piste : la dernière image calculée. `ON DELETE CASCADE` : une
-- piste retirée emporte sa ligne. Une image évincée du cache emporte la sienne
-- (route du spectrogramme), et une ligne dont l'image a disparu s'efface à la
-- lecture de la fiche.
--
-- Mesure recalculable : la bascule SQLite → PostgreSQL ne la recopie pas
-- (`TABLES_NON_COPIEES` de pg_migrate.rs). Jumelle PG : 109.
CREATE TABLE IF NOT EXISTS spectrogrammes (
    track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    fichier TEXT NOT NULL,
    largeur INTEGER NOT NULL,
    hauteur INTEGER NOT NULL,
    octets INTEGER NOT NULL,
    signature TEXT NOT NULL,
    taille_fichier INTEGER,
    mtime_fichier REAL,
    duree_calcul_ms INTEGER,
    calcule_le TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_spectrogrammes_fichier ON spectrogrammes(fichier);
