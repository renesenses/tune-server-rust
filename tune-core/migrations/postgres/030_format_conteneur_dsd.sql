-- #1612 : rendre son conteneur à chaque piste DSD déjà scannée.
--
-- `normalize_format` repliait `dsf` ET `dff` sur « dsd » : la bibliothèque
-- affichait un seul type de fichier pour deux conteneurs. Il ne le fait plus,
-- mais sans cette migration une bibliothèque existante montrerait « DSD »
-- (anciennes lignes) À CÔTÉ de « DSF » (nouvelles) — le défaut d'origine sous
-- un autre nom, et cette fois par notre faute.
--
-- L'extension du fichier est la seule source qui sache lequel des deux c'était :
-- l'information a été perdue à l'écriture, elle se relit sur le chemin.
--
-- Jumelle de la migration SQLite 81. Les deux listes sont SÉPARÉES :
-- `run_migrations` ne prend qu'un `SqliteDb`. Corriger un seul côté laisserait
-- les serveurs PostgreSQL — .15, .18, Docker — avec le défaut entier.
--
-- Idempotent : la clause `format = 'dsd'` ne rattrape que ce qui n'a pas
-- encore été converti.
--
-- ⚠️ `cue_media_path` n'est PAS garantie ici. Elle existe dans le schéma neuf
-- (`pg_migrate.rs`), mais AUCUNE migration `.sql` numérotée ne l'ajoute à une
-- table `tracks` déjà créée : une base PostgreSQL antérieure au chantier CUE
-- (#1763) ne l'a jamais reçue, et le contrôle CI qui applique ces fichiers sur
-- une base nue non plus. La référencer sans garde faisait échouer la migration
-- — et un échec ici arrête tout le train. Le bloc conditionnel ci-dessous la
-- consulte quand elle est là, l'ignore sinon. Le manque lui-même est un défaut
-- distinct, signalé à part.

DO $$
DECLARE
  a_cue BOOLEAN;
  chemin TEXT;
BEGIN
  SELECT EXISTS (
    SELECT 1 FROM information_schema.columns
     WHERE table_name = 'tracks' AND column_name = 'cue_media_path'
  ) INTO a_cue;

  chemin := CASE WHEN a_cue
                 THEN 'COALESCE(file_path, cue_media_path, '''')'
                 ELSE 'COALESCE(file_path, '''')'
            END;

  EXECUTE format(
    'UPDATE tracks SET format = ''dsf'' WHERE format = ''dsd''
       AND LOWER(%s) LIKE ''%%.dsf''', chemin);

  EXECUTE format(
    'UPDATE tracks SET format = ''dff'' WHERE format = ''dsd''
       AND LOWER(%s) LIKE ''%%.dff''', chemin);

  IF NOT a_cue THEN
    RAISE NOTICE 'migration 030 : cue_media_path absente, pistes CUE non converties';
  END IF;
END $$;

-- L'album suit ses pistes : sa colonne `format` est un résumé, et un album dont
-- toutes les pistes sont des `.dsf` est un album DSF. Un album qui mélangerait
-- les deux garde « dsd », qui reste vrai et reste reconnu partout
-- (`IN ('dsd','dsf','dff')`).
UPDATE albums SET format = 'dsf'
 WHERE format = 'dsd'
   AND NOT EXISTS (SELECT 1 FROM tracks t
                    WHERE t.album_id = albums.id AND t.format <> 'dsf')
   AND EXISTS (SELECT 1 FROM tracks t
                WHERE t.album_id = albums.id AND t.format = 'dsf');

UPDATE albums SET format = 'dff'
 WHERE format = 'dsd'
   AND NOT EXISTS (SELECT 1 FROM tracks t
                    WHERE t.album_id = albums.id AND t.format <> 'dff')
   AND EXISTS (SELECT 1 FROM tracks t
                WHERE t.album_id = albums.id AND t.format = 'dff');
