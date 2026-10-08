-- 085_true_peak_algo_etiquette.sql
--
-- #2713 : la crête vraie a sa version. Jumelle de la migration SQLite 121.
--
-- Les crêtes déjà en base (`rg_track_true_peak`, `rg_album_true_peak`)
-- viennent de l'interpolation Catmull-Rom 4× d'avant l'annexe 2 de
-- BS.1770. Elles sont ÉTIQUETÉES `catmull-rom-4x`, pas effacées : elles
-- servent à `prevent_clipping` jusqu'à leur remplacement par le rattrapage
-- de fond. Les gains ne sont pas touchés.
--
-- Idempotent : ON CONFLICT DO NOTHING sur une version déjà posée.

BEGIN;

INSERT INTO track_metadata (track_id, key, value)
SELECT m.track_id, 'rg_true_peak_algo', 'catmull-rom-4x' FROM track_metadata m
WHERE m.key = 'rg_track_true_peak'
ON CONFLICT (track_id, key) DO NOTHING;

INSERT INTO track_metadata (track_id, key, value)
SELECT m.track_id, 'rg_album_true_peak_algo', 'catmull-rom-4x' FROM track_metadata m
WHERE m.key = 'rg_album_true_peak'
ON CONFLICT (track_id, key) DO NOTHING;

INSERT INTO schema_version (version, name) VALUES (85, 'true_peak_algo_etiquette')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
