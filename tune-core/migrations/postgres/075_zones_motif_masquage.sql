-- POURQUOI une zone est masquée (#5077, Villerio, fil 1926).
--
-- Jumelle de la migration SQLite 112. Les deux listes sont SÉPARÉES —
-- `run_migrations` ne prend qu'un `SqliteDb` — donc une colonne posée d'un
-- seul côté ne répare que la moitié du parc (#1612, #2111). Et celles-ci sont
-- NOMMÉES par chaque masquage et chaque démasquage de zone.
--
-- `motif_masquage` : une valeur de `zone_motif_masquage::MotifMasquage`
-- (`suppression_utilisateur`, `suppression_totale`, `doublon_local_generique`,
-- `appareil_ignore`, `zone_reflet`, `fusion`, `autre`). `masquee_le` : l'heure
-- du masquage, ISO 8601 UTC, TEXT comme `zones.last_seen_at`.
--
-- 🔴 NUL = INCONNU, pour TOUT masquage existant : rien ne dit pourquoi une
-- zone déjà masquée l'a été. Un motif inconnu n'est jamais démasqué
-- automatiquement.
--
-- Numérotée 075, PAS 073 : la 073 est prise par #4925 (exemplaires par
-- répertoire), la 074 par #5034 (source de pochette), PR ouvertes en même
-- temps. Le lanceur ne joue que `version > MAX` : cette migration EXIGE la 073
-- et la 074 avant elle, sinon elle se renumérote à la promotion.
--
-- Idempotent : `IF NOT EXISTS`, sans danger à rejouer sur une base déjà
-- migrée depuis SQLite.

BEGIN;

ALTER TABLE zones
    ADD COLUMN IF NOT EXISTS motif_masquage TEXT;
ALTER TABLE zones
    ADD COLUMN IF NOT EXISTS masquee_le TEXT;

INSERT INTO schema_version (version, name) VALUES (75, 'zones_motif_masquage')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
