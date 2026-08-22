-- #1612 : « DSD » apparaissait deux fois dans les types de fichiers.
--
-- La facette regroupe désormais en LOWER(TRIM()) côté requête, ce qui corrige
-- l'AFFICHAGE sans toucher aux données. Mais les filtres, eux, comparent la
-- valeur EXACTE : tant que `dsd` et `DSD` coexistent en lignes, cliquer sur
-- « DSD » ne rend que la moitié des albums. L'écran cesserait de mentir
-- pendant que le filtre continuerait — soit le pire des deux états.
--
-- Jumelle de la migration SQLite 78 (`format_lowercase`). Les deux moteurs ont
-- des listes de migrations SÉPARÉES : `run_migrations` ne prend qu'un
-- `SqliteDb`, et PostgreSQL a ces fichiers numérotés. Corriger un seul côté
-- laisserait les serveurs PostgreSQL — dont .15 et .18 — avec le défaut
-- entier.
--
-- Idempotent par construction : LOWER(LOWER(x)) vaut LOWER(x). La clause WHERE
-- évite de réécrire les lignes déjà propres, ce qui compte sur une
-- bibliothèque de plusieurs dizaines de milliers d'albums.

UPDATE albums
   SET format = LOWER(TRIM(format))
 WHERE format IS NOT NULL
   AND format <> LOWER(TRIM(format));

UPDATE tracks
   SET format = LOWER(TRIM(format))
 WHERE format IS NOT NULL
   AND format <> LOWER(TRIM(format));
