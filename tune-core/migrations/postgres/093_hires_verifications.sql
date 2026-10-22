-- 093_hires_verifications.sql
--
-- #6285 : la détection des faux hi-res (CD gonflé en 24/96, MP3 converti en
-- FLAC). Le verdict de chaque piste, la coupure spectrale et les bits
-- effectifs mesurés sur un extrait. Jumelle de la migration SQLite 129.
--
-- La table naît VIDE : la passe de fond (désactivée par défaut) la remplit.
-- Elle n'est pas recopiée à la bascule SQLite → PostgreSQL (mesures
-- recalculables, `TABLES_NON_COPIEES` de pg_migrate.rs).
--
-- Idempotent : CREATE TABLE / INDEX IF NOT EXISTS sont sûrs à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS hires_verifications (
    track_id BIGINT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    verdict TEXT NOT NULL,
    raison TEXT,
    coupure_hz DOUBLE PRECISION,
    bits_effectifs BIGINT,
    sample_rate BIGINT,
    bits_declares BIGINT,
    mesure_le TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);
CREATE INDEX IF NOT EXISTS idx_hires_verifications_verdict ON hires_verifications(verdict);

INSERT INTO schema_version (version, name) VALUES (93, 'hires_verifications')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
