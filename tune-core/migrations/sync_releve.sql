-- #8239 — le relevé de la synchro cloud : pour chaque piste, album et
-- artiste, les valeurs qui partent vers le cloud telles qu'elles étaient au
-- dernier passage de `library_sync::journaliser_ce_qui_a_change`. La fin d'un
-- scan ne met au journal (`sync_changelog`) que la différence avec ce relevé,
-- au lieu de toute la bibliothèque.
--
-- `sync_releve_*` : le relevé. `sync_releve_*_neuf` : l'état courant, figé le
-- temps de la comparaison, vide en dehors d'elle. Les colonnes reprennent la
-- projection de `FAMILLES_SUIVIES` (library_sync.rs) : les changer ensemble.
--
-- Même texte pour SQLite (144) et PostgreSQL (110).
CREATE TABLE IF NOT EXISTS sync_releve_pistes (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 album TEXT,
 album_id BIGINT,
 format TEXT,
 sample_rate BIGINT,
 bit_depth BIGINT,
 duration_ms BIGINT,
 genre TEXT,
 track_number BIGINT,
 disc_number BIGINT,
 source TEXT,
 source_id TEXT,
 isrc TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_pistes_neuf (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 album TEXT,
 album_id BIGINT,
 format TEXT,
 sample_rate BIGINT,
 bit_depth BIGINT,
 duration_ms BIGINT,
 genre TEXT,
 track_number BIGINT,
 disc_number BIGINT,
 source TEXT,
 source_id TEXT,
 isrc TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_albums (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 annee BIGINT,
 genre TEXT,
 source TEXT,
 nb_pistes BIGINT,
 mbid_groupe TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_albums_neuf (
 id BIGINT PRIMARY KEY,
 titre TEXT,
 artiste TEXT,
 annee BIGINT,
 genre TEXT,
 source TEXT,
 nb_pistes BIGINT,
 mbid_groupe TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_artistes (
 id BIGINT PRIMARY KEY,
 nom TEXT,
 bio TEXT,
 mbid TEXT
);
CREATE TABLE IF NOT EXISTS sync_releve_artistes_neuf (
 id BIGINT PRIMARY KEY,
 nom TEXT,
 bio TEXT,
 mbid TEXT
);
