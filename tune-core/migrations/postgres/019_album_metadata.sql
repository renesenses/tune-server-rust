-- 019_album_metadata.sql
--
-- Store clé/valeur d'extra metadata au niveau ALBUM (SQLite migration v68),
-- symétrique de track_metadata. Jusqu'ici l'UI web stockait les champs album
-- du Vademecum (conductor, performer, barcode…) sur la PREMIÈRE piste de
-- l'album faute d'endpoint album-level — donnée invisible depuis les autres
-- pistes et perdue si la piste 1 est re-scannée. Servi par
-- GET/PUT /api/v1/library/albums/{id}/metadata.

BEGIN;

CREATE TABLE IF NOT EXISTS album_metadata (
    album_id BIGINT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (album_id, key)
);

CREATE INDEX IF NOT EXISTS idx_album_metadata_key ON album_metadata(key);

INSERT INTO schema_version (version, name) VALUES (19, 'album_metadata')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
