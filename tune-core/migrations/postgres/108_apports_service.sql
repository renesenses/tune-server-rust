-- 108_apports_service.sql
--
-- #8077 : jumelle de la migration SQLite 142 (même DDL,
-- tune-core/migrations/apports_service.sql). Idempotent.

BEGIN;

CREATE TABLE IF NOT EXISTS apports_service_albums (
 album_id BIGINT NOT NULL,
 source TEXT NOT NULL,
 service_album_id TEXT,
 methode TEXT,
 statut TEXT NOT NULL,
 motif TEXT,
 pistes_locales BIGINT NOT NULL DEFAULT 0,
 pistes_rapprochees BIGINT NOT NULL DEFAULT 0,
 verifie_le TEXT NOT NULL,
 PRIMARY KEY (album_id, source)
);
CREATE TABLE IF NOT EXISTS apports_service (
 album_id BIGINT NOT NULL,
 track_id BIGINT NOT NULL DEFAULT 0,
 champ TEXT NOT NULL,
 source TEXT NOT NULL,
 valeur TEXT NOT NULL,
 cree_le TEXT NOT NULL,
 PRIMARY KEY (album_id, track_id, champ, source)
);
CREATE INDEX IF NOT EXISTS idx_apports_service_piste ON apports_service(track_id)
;

INSERT INTO schema_version (version, name) VALUES (108, 'apports_service')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
