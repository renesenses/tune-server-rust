-- Le TYPE DE SORTIE du disque : album, EP ou single.
--
-- Jumelle de la migration SQLite 106 (#4767). Les deux listes sont SEPAREES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posee d'un
-- seul cote ne repare que la moitie du parc (#1612, #2111). Et celle-ci est
-- NOMMEE par `album_repo::sql::select_album`, le SELECT commun de TOUS les
-- ecrans d'albums : sans ce script, une base PostgreSQL rendrait une
-- bibliotheque vide, partout.
--
-- Sans cette colonne, la frontiere entre « Albums principaux » et
-- « EP & singles » demandee par FabienM n'existe pas : ni la table `albums`,
-- ni le scan, ni l'enrichissement ne portaient l'information.
--
-- SOURCE : `primary-type` du GROUPE DE SORTIE MusicBrainz, dont
-- `albums.musicbrainz_release_group_id` porte deja l'identifiant. Les
-- `secondary-types` (Live, Compilation, Soundtrack, Remix…) ne changent JAMAIS
-- ce type : un album live reste un album.
--
-- 🔴 NUL = INCONNU, et l'inconnu est l'etat NORMAL. La couverture MBID mesuree
-- est de 0,9 % sur le .18 et 88,4 % sur le .15 : sur la plupart des disques,
-- MusicBrainz ne repondra pas. Aucune heuristique ne remplit cette colonne —
-- ni le nombre de titres, ni la duree totale. Un tri faux est pire qu'une
-- section absente.
--
-- TEXT, sans defaut, sur les deux moteurs : la valeur est un mot du
-- vocabulaire MusicBrainz mis en bas de casse (`album`, `ep`, `single`,
-- `broadcast`, `other`), pas un booleen — donc rien a ramener a un SMALLINT
-- apres coup, contrairement a `is_compilation` (migration PG 028).
--
-- Idempotent : `IF NOT EXISTS`, sans danger a rejouer sur une base deja migree
-- depuis SQLite. Les lignes existantes gardent NULL — cette base n'a jamais su
-- le type d'un disque, et rien ici ne pretend le reconstituer.

BEGIN;

ALTER TABLE albums
    ADD COLUMN IF NOT EXISTS release_type TEXT;

INSERT INTO schema_version (version, name) VALUES (69, 'albums_type_de_sortie')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
