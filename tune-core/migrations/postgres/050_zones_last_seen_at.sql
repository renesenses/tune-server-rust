-- 050_zones_last_seen_at.sql
--
-- DUP-1 (phase 2) : la dernière fois qu'un appareil a répondu, par zone,
-- en ISO 8601 UTC.
--
-- `online` n'a qu'un état : une zone éteinte depuis deux minutes et une zone
-- abandonnée depuis trois semaines portaient la même ligne. La colonne n'est
-- écrite qu'au passage EN LIGNE, jamais au passage hors ligne, et reste NULL
-- pour les lignes existantes (« jamais vue depuis la mise à jour ») — poser
-- la date d'installation sur une zone morte la ferait passer pour récente.
--
-- TEXT comme côté SQLite : rien à rattraper dans la parité de types.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE zones
    ADD COLUMN IF NOT EXISTS last_seen_at TEXT;

INSERT INTO schema_version (version, name) VALUES (50, 'zones_last_seen_at')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
