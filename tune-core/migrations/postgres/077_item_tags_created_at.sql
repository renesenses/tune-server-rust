-- 077_item_tags_created_at.sql
--
-- #5478 (web#1802, « Écouter plus tard ») : la date du DÉPÔT d'un objet local
-- dans une étiquette. Jumelle de la migration SQLite 113.
--
-- `streaming_item_tags` porte `created_at` depuis la 052 ; `item_tags` n'en
-- avait aucune, et l'écran ne pouvait pas trier par « ajouté récemment ».
--
-- NULL pour les lignes existantes, jamais now() : dater de la mise à jour un
-- dépôt ancien le ferait passer pour le plus récent. La colonne est remplie à
-- l'étiquetage (`tag_repo::sql::tag_item`) et n'est jamais réécrite.
--
-- TEXT ISO-8601 UTC comme côté SQLite et comme `streaming_item_tags` : rien à
-- rattraper dans la parité de types.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE item_tags
    ADD COLUMN IF NOT EXISTS created_at TEXT;

INSERT INTO schema_version (version, name) VALUES (77, 'item_tags_created_at')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
