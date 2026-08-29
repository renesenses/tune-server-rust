-- #2713 : les pics calcules par Tune etaient des sample peaks clamps a 1.0.
-- BS.1770-5 reconstruit desormais le signal a >= 192 kHz et conserve les
-- depassements inter-echantillons.
--
-- `rg_analyzed` n'est pas une preuve generique de traitement : un ENOENT est
-- desormais reporte sans ce temoin (#1865). Ici il ne sert que de provenance
-- LEGACY, conjointement a un resultat ReplayGain existant. Les tags de fichier,
-- qui n'ont jamais recu le temoin Tune, sont preserves.

BEGIN;

CREATE TEMP TABLE tune_true_peak_reanalysis ON COMMIT DROP AS
SELECT DISTINCT a.track_id
FROM track_metadata a
WHERE a.key = 'rg_analyzed'
  AND EXISTS (
      SELECT 1 FROM track_metadata value
      WHERE value.track_id = a.track_id
        AND value.key IN ('rg_track_gain', 'rg_track_peak')
  );

DELETE FROM track_metadata value
USING tune_true_peak_reanalysis affected
WHERE value.track_id = affected.track_id
  AND value.key IN ('rg_analyzed', 'rg_track_gain', 'rg_track_peak',
                    'rg_album_gain', 'rg_album_peak',
                    'rg_track_analysis_version', 'rg_album_analysis_version');

INSERT INTO schema_version (version, name)
VALUES (41, 'replaygain_true_peak_bs1770_5')
ON CONFLICT (version) DO NOTHING;

COMMIT;
