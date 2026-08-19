-- 027_network_mounts_mount_state.sql
--
-- Le CONSTAT du dernier montage SMB, à côté de l'INTENTION que porte `active`.
--
-- `smb_version` retient le dialecte qui a réellement monté le partage. Le
-- remontage au démarrage imposait `vers=3.0` en dur (#1834) : le partage de
-- Philippe Landes, exposé par un streamer Rose qui ne parle que SMB 1.0,
-- montait depuis l'assistant — qui, lui, avait appris à négocier — puis se
-- perdait au premier redémarrage. Sans mémoire du dialecte, chaque démarrage
-- rejouait la même impasse.
--
-- `mount_state` et `last_mount_error` rendent l'échec visible (#1916). Il ne
-- l'était nulle part : l'interface continuait d'afficher le partage comme
-- monté, la bibliothèque s'affichait normalement puisque les pistes sont en
-- base, et la lecture rendait une erreur réseau générique. Éric (ricouxxx) a
-- dû trouver seul le contournement, sur un fil de forum public.
--
-- Idempotent : ADD COLUMN IF NOT EXISTS est sûr à rejouer.

BEGIN;

ALTER TABLE network_mounts
    ADD COLUMN IF NOT EXISTS smb_version TEXT,
    ADD COLUMN IF NOT EXISTS mount_state TEXT,
    ADD COLUMN IF NOT EXISTS last_mount_error TEXT;

INSERT INTO schema_version (version, name) VALUES (27, 'network_mounts_mount_state')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
