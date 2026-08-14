-- #1638 : le décimateur DSD→PCM applique désormais l'échelle SACD (+6 dB).
-- Les ReplayGain calculés par NOTRE analyse sur l'ancienne échelle sont faux
-- de ~6 dB : on les efface pour que le sweep les recalcule. Portée stricte :
-- 1) les pistes DSD passées par l'analyse (sentinelle rg_analyzed) — les RG
--    venus des TAGS du fichier (pas de sentinelle) sont préservés ;
-- 2) les clés d'ALBUM de tout album contenant une telle piste (le gain
--    d'album mêle les LUFS de toutes les pistes) — sans toucher aux gains de
--    PISTE des voisines PCM.
DELETE FROM track_metadata
WHERE key IN ('rg_analyzed','rg_track_gain','rg_track_peak','rg_album_gain','rg_album_peak','rg_skipped_oversized')
  AND track_id IN (
    SELECT t.id FROM tracks t
    JOIN track_metadata m ON m.track_id = t.id AND m.key = 'rg_analyzed'
    WHERE lower(COALESCE(t.format,'')) IN ('dsd','dsf','dff','dsdiff')
       OR lower(t.file_path) LIKE '%.dsf'
       OR lower(t.file_path) LIKE '%.dff'
  );

DELETE FROM track_metadata
WHERE key IN ('rg_album_gain','rg_album_peak')
  AND track_id IN (
    SELECT t2.id FROM tracks t2
    WHERE t2.album_id IS NOT NULL AND t2.album_id IN (
      SELECT t.album_id FROM tracks t
      WHERE (lower(COALESCE(t.format,'')) IN ('dsd','dsf','dff','dsdiff')
          OR lower(t.file_path) LIKE '%.dsf'
          OR lower(t.file_path) LIKE '%.dff')
        AND t.album_id IS NOT NULL
    )
  );
