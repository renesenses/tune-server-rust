-- Banc pour une base PostgreSQL de test initialisée, migration 060 appliquée.
-- Chaque mutation des données est annulée en fin de script.
\set ON_ERROR_STOP on
BEGIN;
DO $$
DECLARE
  before_value BIGINT;
  statement TEXT;
BEGIN
  FOREACH statement IN ARRAY ARRAY[
    'INSERT INTO artists (id,name) VALUES (98765,''Artiste'')',
    'INSERT INTO albums (id,title,artist_id) VALUES (98765,''Album'',98765)',
    'INSERT INTO tracks (id,title,album_id) VALUES (98765,''Piste'',98765)',
    'INSERT INTO playlists (id,name,profile_id) VALUES (98765,''Liste'',1)',
    'INSERT INTO playlist_tracks (id,playlist_id,track_id,position) VALUES (98765,98765,98765,0)',
    'INSERT INTO radio_stations (id,name,url) VALUES (98765,''Radio'',''http://exemple'')',
    'INSERT INTO hidden_items (profile_id,item_type,item_id) VALUES (1,''album'',''98765'')',
    'INSERT INTO track_metadata (track_id,key,value) VALUES (98765,''upnp_res_url'',''http://nas/piste'')',
    'UPDATE albums SET cover_path = ''nouvelle'' WHERE id = ''98765''',
    'UPDATE artists SET name = ''Autre artiste'' WHERE id = ''98765''',
    'UPDATE artists SET sort_name = ''Tri'' WHERE id = ''98765''',
    'UPDATE radio_stations SET is_favorite = ''1'' WHERE id = ''98765''',
    'UPDATE playlist_tracks SET position = 1 WHERE playlist_id = ''98765''',
    'DELETE FROM hidden_items WHERE item_id = ''98765'''
  ] LOOP
    SELECT value INTO before_value FROM upnp_catalog_revision WHERE id=1;
    EXECUTE statement;
    IF (SELECT value FROM upnp_catalog_revision WHERE id=1) = before_value THEN
      RAISE EXCEPTION 'Modification non annoncée : %', statement;
    END IF;
  END LOOP;
  SELECT value INTO before_value FROM upnp_catalog_revision WHERE id=1;
  UPDATE tracks SET title=title, file_mtime=123, comments='note' WHERE id='98765';
  INSERT INTO track_metadata (track_id,key,value) VALUES (98765,'rg_track_gain','-3');
  UPDATE radio_stations SET play_count=42 WHERE id='98765';
  IF (SELECT value FROM upnp_catalog_revision WHERE id=1) <> before_value THEN
    RAISE EXCEPTION 'Réécriture identique ou analyse acoustique change le compteur';
  END IF;
  BEGIN
    UPDATE tracks SET title='Annulé' WHERE id='98765';
    RAISE EXCEPTION 'annulation';
  EXCEPTION WHEN raise_exception THEN NULL;
  END;
  IF (SELECT value FROM upnp_catalog_revision WHERE id=1) <> before_value THEN
    RAISE EXCEPTION 'Compteur non annulé avec la transaction';
  END IF;
  UPDATE upnp_catalog_revision SET value=4294967295 WHERE id=1;
  DELETE FROM tracks WHERE id='98765';
  IF (SELECT value FROM upnp_catalog_revision WHERE id=1) <> 0 THEN
    RAISE EXCEPTION 'Débordement ui4 incorrect';
  END IF;
  RAISE NOTICE 'OK : 8 familles, UPDATE identique, analyse ignorée, rollback et débordement ui4';
END $$;
ROLLBACK;
