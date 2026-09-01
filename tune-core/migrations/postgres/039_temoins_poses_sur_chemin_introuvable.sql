-- Jumelle PostgreSQL de la migration SQLite 87 (#1865).
--
-- Rendre leur chance aux pistes que le repli NFC/NFD va desormais retrouver.
--
-- Le scanner enregistre les chemins en NFC ; macOS et les partages SMB/CIFS
-- ecrivent les noms de fichiers en NFD. Les passes de fond ouvraient le chemin
-- de la base TEL QUEL, recevaient ENOENT, et posaient quand meme leur temoin
-- (« on a essaye, n'y revenons pas ») : la piste sortait du balayage pour
-- toujours. Mesure sur .18 le 28/08/2026 : 135 pistes dont le chemin stocke ne
-- resout pas mais dont la forme NFD existe ; 114 portaient `rg_analyzed` pour
-- ZERO `rg_track_gain`, 44 portaient `audio_embed_analyzed` pour ZERO vecteur.
--
-- Le code ne pose plus ces temoins sur un fichier introuvable — il pose un
-- report date, qui perime. Mais il ne rattrape rien tout seul : les temoins
-- deja en base excluent ces pistes de la requete de candidats. D'ou ce
-- nettoyage.
--
-- Predicat volontairement etroit : le temoin ne saute que la ou il ne recouvre
-- AUCUN resultat ET ou le chemin porte au moins un octet non-ASCII (seuls les
-- chemins accentues ont deux graphies Unicode possibles). Un fichier vraiment
-- illisible au nom accentue sera reessaye UNE fois, puis re-temoigne.
--
-- `octet_length` vs `length` : octets contre caracteres. Leur inegalite est le
-- test non-ASCII, sans regex ni collation. (La jumelle SQLite emploie
-- `length(CAST(x AS BLOB))`, `octet_length` n'existant pas sur les SQLite
-- anciens.)
--
-- IDEMPOTENTE : ce sont des DELETE. Rejouee, elle ne trouve plus rien.
-- `rg_skipped_oversized` est epargne — ce refus vient d'un calcul sur la duree
-- et le debit, pas d'un acces disque.

DELETE FROM track_metadata
WHERE key = 'rg_analyzed'
  AND track_id IN (
    SELECT t.id FROM tracks t
    WHERE t.file_path IS NOT NULL
      AND octet_length(t.file_path) <> length(t.file_path)
      AND NOT EXISTS (SELECT 1 FROM track_metadata g
                       WHERE g.track_id = t.id AND g.key = 'rg_track_gain')
      AND NOT EXISTS (SELECT 1 FROM track_metadata s
                       WHERE s.track_id = t.id AND s.key = 'rg_skipped_oversized')
  );

DELETE FROM track_metadata
WHERE key = 'audio_embed_analyzed'
  AND track_id IN (
    SELECT t.id FROM tracks t
    WHERE t.file_path IS NOT NULL
      AND octet_length(t.file_path) <> length(t.file_path)
      AND NOT EXISTS (SELECT 1 FROM track_audio_embedding e
                       WHERE e.track_id = t.id)
  );
