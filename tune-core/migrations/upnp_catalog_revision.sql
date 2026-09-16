-- #4201 : compteur du catalogue publié, atomique avec ses modifications.
-- Les UPDATE sans changement des champs suivis ne changent pas le compteur.

CREATE TABLE IF NOT EXISTS upnp_catalog_revision (id INTEGER PRIMARY KEY CHECK (id = 1), value BIGINT NOT NULL CHECK (value >= 0 AND value <= 4294967295));

INSERT INTO upnp_catalog_revision (id,value) VALUES (1,0) ON CONFLICT(id) DO NOTHING;

CREATE TRIGGER IF NOT EXISTS upnp_revision_tracks_insert AFTER INSERT ON tracks
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_tracks_delete AFTER DELETE ON tracks
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_tracks_update AFTER UPDATE ON tracks WHEN (OLD.id IS NOT NEW.id
    OR OLD.title IS NOT NEW.title
    OR OLD.album_id IS NOT NEW.album_id
    OR OLD.artist_id IS NOT NEW.artist_id
    OR OLD.disc_number IS NOT NEW.disc_number
    OR OLD.track_number IS NOT NEW.track_number
    OR OLD.duration_ms IS NOT NEW.duration_ms
    OR OLD.file_path IS NOT NEW.file_path
    OR OLD.format IS NOT NEW.format
    OR OLD.sample_rate IS NOT NEW.sample_rate
    OR OLD.bit_depth IS NOT NEW.bit_depth
    OR OLD.channels IS NOT NEW.channels
    OR OLD.file_size IS NOT NEW.file_size
    OR OLD.source IS NOT NEW.source
    OR OLD.source_id IS NOT NEW.source_id
    OR OLD.cover_path IS NOT NEW.cover_path
    OR OLD.genre IS NOT NEW.genre
    OR OLD.genres IS NOT NEW.genres
    OR OLD.year IS NOT NEW.year)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_albums_insert AFTER INSERT ON albums
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_albums_delete AFTER DELETE ON albums
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_albums_update AFTER UPDATE ON albums WHEN (OLD.id IS NOT NEW.id
    OR OLD.title IS NOT NEW.title
    OR OLD.artist_id IS NOT NEW.artist_id
    OR OLD.year IS NOT NEW.year
    OR OLD.genre IS NOT NEW.genre
    OR OLD.genres IS NOT NEW.genres
    OR OLD.disc_count IS NOT NEW.disc_count
    OR OLD.track_count IS NOT NEW.track_count
    OR OLD.cover_path IS NOT NEW.cover_path
    OR OLD.source IS NOT NEW.source
    OR OLD.source_id IS NOT NEW.source_id
    OR OLD.format IS NOT NEW.format
    OR OLD.sample_rate IS NOT NEW.sample_rate
    OR OLD.bit_depth IS NOT NEW.bit_depth)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_artists_insert AFTER INSERT ON artists
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_artists_delete AFTER DELETE ON artists
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_artists_update AFTER UPDATE ON artists WHEN (OLD.sort_name IS NOT NEW.sort_name
    OR OLD.id IS NOT NEW.id
    OR OLD.name IS NOT NEW.name)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_playlists_insert AFTER INSERT ON playlists
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_playlists_delete AFTER DELETE ON playlists
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_playlists_update AFTER UPDATE ON playlists WHEN (OLD.id IS NOT NEW.id
    OR OLD.name IS NOT NEW.name
    OR OLD.profile_id IS NOT NEW.profile_id)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_playlist_tracks_insert AFTER INSERT ON playlist_tracks
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_playlist_tracks_delete AFTER DELETE ON playlist_tracks
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_playlist_tracks_update AFTER UPDATE ON playlist_tracks WHEN (OLD.playlist_id IS NOT NEW.playlist_id
    OR OLD.track_id IS NOT NEW.track_id
    OR OLD.position IS NOT NEW.position)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_radio_stations_insert AFTER INSERT ON radio_stations
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_radio_stations_delete AFTER DELETE ON radio_stations
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_radio_stations_update AFTER UPDATE ON radio_stations WHEN (OLD.is_favorite IS NOT NEW.is_favorite
    OR OLD.id IS NOT NEW.id
    OR OLD.name IS NOT NEW.name
    OR OLD.url IS NOT NEW.url
    OR OLD.logo_url IS NOT NEW.logo_url
    OR OLD.genre IS NOT NEW.genre
    OR OLD.country IS NOT NEW.country)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_hidden_items_insert AFTER INSERT ON hidden_items
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_hidden_items_delete AFTER DELETE ON hidden_items
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_hidden_items_update AFTER UPDATE ON hidden_items WHEN (OLD.profile_id IS NOT NEW.profile_id
    OR OLD.item_type IS NOT NEW.item_type
    OR OLD.item_id IS NOT NEW.item_id)
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_track_metadata_insert AFTER INSERT ON track_metadata WHEN (NEW.key = 'upnp_res_url')
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_track_metadata_delete AFTER DELETE ON track_metadata WHEN (OLD.key = 'upnp_res_url')
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS upnp_revision_track_metadata_update AFTER UPDATE ON track_metadata WHEN ((OLD.track_id IS NOT NEW.track_id
    OR OLD.key IS NOT NEW.key
    OR OLD.value IS NOT NEW.value) AND (OLD.key = 'upnp_res_url'
    OR NEW.key = 'upnp_res_url'))
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
END;
