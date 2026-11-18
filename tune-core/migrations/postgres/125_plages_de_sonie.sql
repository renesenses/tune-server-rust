-- 125_plages_de_sonie.sql
--
-- #9040 : la plage de sonie EBU R128 (LRA) de chaque piste, et l'histogramme
-- de ses sonies à court terme (cases de 0,1 LU) d'où se calcule la LRA d'un
-- album. Jumelle de la migration SQLite 158 (RC14 ; 123 et 124 sont aux
-- PR #8845 et #8912 du lot rc13).
--
-- La table naît VIDE : la passe ReplayGain et le rattrapage du DR la
-- remplissent. Recopiée à la bascule SQLite → PostgreSQL
-- (`MIGRATION_TABLES_APRES_SCRIPTS` de pg_migrate.rs).
--
-- Idempotent : CREATE TABLE IF NOT EXISTS est sûr à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS plages_de_sonie (
    track_id BIGINT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    lra_lu DOUBLE PRECISION,
    histogramme TEXT NOT NULL,
    algo TEXT NOT NULL,
    mesure_le TEXT NOT NULL
);

INSERT INTO schema_version (version, name) VALUES (125, 'plages_de_sonie')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
