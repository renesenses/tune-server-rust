-- #4201 : compteur du catalogue publié, atomique avec ses modifications.
-- Les UPDATE sans changement des champs suivis ne changent pas le compteur.

BEGIN;

CREATE TABLE IF NOT EXISTS upnp_catalog_revision (id INTEGER PRIMARY KEY CHECK (id = 1), value BIGINT NOT NULL CHECK (value >= 0 AND value <= 4294967295));

INSERT INTO upnp_catalog_revision (id,value) VALUES (1,0) ON CONFLICT(id) DO NOTHING;

CREATE
    OR REPLACE FUNCTION upnp_catalog_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
    RETURN NULL;
END;
$$;

CREATE
    OR REPLACE TRIGGER upnp_revision_tracks_insert AFTER INSERT ON tracks FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_tracks_delete AFTER DELETE ON tracks FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_tracks_update AFTER UPDATE ON tracks FOR EACH ROW WHEN (OLD.id IS DISTINCT FROM NEW.id
    OR OLD.title IS DISTINCT FROM NEW.title
    OR OLD.album_id IS DISTINCT FROM NEW.album_id
    OR OLD.artist_id IS DISTINCT FROM NEW.artist_id
    OR OLD.disc_number IS DISTINCT FROM NEW.disc_number
    OR OLD.track_number IS DISTINCT FROM NEW.track_number
    OR OLD.duration_ms IS DISTINCT FROM NEW.duration_ms
    OR OLD.file_path IS DISTINCT FROM NEW.file_path
    OR OLD.format IS DISTINCT FROM NEW.format
    OR OLD.sample_rate IS DISTINCT FROM NEW.sample_rate
    OR OLD.bit_depth IS DISTINCT FROM NEW.bit_depth
    OR OLD.channels IS DISTINCT FROM NEW.channels
    OR OLD.file_size IS DISTINCT FROM NEW.file_size
    OR OLD.source IS DISTINCT FROM NEW.source
    OR OLD.source_id IS DISTINCT FROM NEW.source_id
    OR OLD.cover_path IS DISTINCT FROM NEW.cover_path
    OR OLD.genre IS DISTINCT FROM NEW.genre
    OR OLD.genres IS DISTINCT FROM NEW.genres
    OR OLD.year IS DISTINCT FROM NEW.year) EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_albums_insert AFTER INSERT ON albums FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_albums_delete AFTER DELETE ON albums FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_albums_update AFTER UPDATE ON albums FOR EACH ROW WHEN (OLD.id IS DISTINCT FROM NEW.id
    OR OLD.title IS DISTINCT FROM NEW.title
    OR OLD.artist_id IS DISTINCT FROM NEW.artist_id
    OR OLD.year IS DISTINCT FROM NEW.year
    OR OLD.genre IS DISTINCT FROM NEW.genre
    OR OLD.genres IS DISTINCT FROM NEW.genres
    OR OLD.disc_count IS DISTINCT FROM NEW.disc_count
    OR OLD.track_count IS DISTINCT FROM NEW.track_count
    OR OLD.cover_path IS DISTINCT FROM NEW.cover_path
    OR OLD.source IS DISTINCT FROM NEW.source
    OR OLD.source_id IS DISTINCT FROM NEW.source_id
    OR OLD.format IS DISTINCT FROM NEW.format
    OR OLD.sample_rate IS DISTINCT FROM NEW.sample_rate
    OR OLD.bit_depth IS DISTINCT FROM NEW.bit_depth) EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_artists_insert AFTER INSERT ON artists FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_artists_delete AFTER DELETE ON artists FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_artists_update AFTER UPDATE ON artists FOR EACH ROW WHEN (OLD.sort_name IS DISTINCT FROM NEW.sort_name
    OR OLD.id IS DISTINCT FROM NEW.id
    OR OLD.name IS DISTINCT FROM NEW.name) EXECUTE FUNCTION upnp_catalog_changed();

-- `playlists.profile_id` n'existe pas dans les scripts SQL numérotés : sur un
-- serveur réel, `ENSURE_TABLES` (pg_migrate.rs) la pose au démarrage AVANT ces
-- scripts ; sur une base rejouée par les seuls scripts — la CI PostgreSQL —
-- elle manque, et le déclencheur ci-dessous refusait `OLD.profile_id`.
-- Idempotent : déjà là sur tout serveur en service.
ALTER TABLE playlists ADD COLUMN IF NOT EXISTS profile_id BIGINT NOT NULL DEFAULT 1;

CREATE
    OR REPLACE TRIGGER upnp_revision_playlists_insert AFTER INSERT ON playlists FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_playlists_delete AFTER DELETE ON playlists FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_playlists_update AFTER UPDATE ON playlists FOR EACH ROW WHEN (OLD.id IS DISTINCT FROM NEW.id
    OR OLD.name IS DISTINCT FROM NEW.name
    OR OLD.profile_id IS DISTINCT FROM NEW.profile_id) EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_playlist_tracks_insert AFTER INSERT ON playlist_tracks FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_playlist_tracks_delete AFTER DELETE ON playlist_tracks FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_playlist_tracks_update AFTER UPDATE ON playlist_tracks FOR EACH ROW WHEN (OLD.playlist_id IS DISTINCT FROM NEW.playlist_id
    OR OLD.track_id IS DISTINCT FROM NEW.track_id
    OR OLD.position IS DISTINCT FROM NEW.position) EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_radio_stations_insert AFTER INSERT ON radio_stations FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_radio_stations_delete AFTER DELETE ON radio_stations FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

-- 🔴 Pas de clause WHEN ici : un WHEN qui nomme `is_favorite` enregistre une
-- DÉPENDANCE de colonne, et PostgreSQL refuse alors tout changement de type
-- de cette colonne (« cannot alter type of a column used in a trigger
-- definition »). Or `is_favorite` est précisément la colonne que 062
-- (radio_favorite_integer) convertit sur les bases héritées, et que le banc
-- pg_3181 bascule TEXT ↔ SMALLINT. Le même filtre vit donc DANS la fonction,
-- lu par `to_jsonb` : aucune dépendance, aucun plan figé sur un type.
CREATE
    OR REPLACE FUNCTION upnp_radio_stations_changed() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    avant JSONB := to_jsonb(OLD);
    apres JSONB := to_jsonb(NEW);
BEGIN
    IF avant -> 'is_favorite' IS DISTINCT FROM apres -> 'is_favorite'
        OR avant -> 'id' IS DISTINCT FROM apres -> 'id'
        OR avant -> 'name' IS DISTINCT FROM apres -> 'name'
        OR avant -> 'url' IS DISTINCT FROM apres -> 'url'
        OR avant -> 'logo_url' IS DISTINCT FROM apres -> 'logo_url'
        OR avant -> 'genre' IS DISTINCT FROM apres -> 'genre'
        OR avant -> 'country' IS DISTINCT FROM apres -> 'country' THEN
        UPDATE upnp_catalog_revision SET value = (value + 1) % 4294967296 WHERE id = 1;
    END IF;
    RETURN NULL;
END;
$$;

CREATE
    OR REPLACE TRIGGER upnp_revision_radio_stations_update AFTER UPDATE ON radio_stations FOR EACH ROW EXECUTE FUNCTION upnp_radio_stations_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_hidden_items_insert AFTER INSERT ON hidden_items FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_hidden_items_delete AFTER DELETE ON hidden_items FOR EACH ROW EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_hidden_items_update AFTER UPDATE ON hidden_items FOR EACH ROW WHEN (OLD.profile_id IS DISTINCT FROM NEW.profile_id
    OR OLD.item_type IS DISTINCT FROM NEW.item_type
    OR OLD.item_id IS DISTINCT FROM NEW.item_id) EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_track_metadata_insert AFTER INSERT ON track_metadata FOR EACH ROW WHEN (NEW.key = 'upnp_res_url') EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_track_metadata_delete AFTER DELETE ON track_metadata FOR EACH ROW WHEN (OLD.key = 'upnp_res_url') EXECUTE FUNCTION upnp_catalog_changed();

CREATE
    OR REPLACE TRIGGER upnp_revision_track_metadata_update AFTER UPDATE ON track_metadata FOR EACH ROW WHEN ((OLD.track_id IS DISTINCT FROM NEW.track_id
    OR OLD.key IS DISTINCT FROM NEW.key
    OR OLD.value IS DISTINCT FROM NEW.value) AND (OLD.key = 'upnp_res_url'
    OR NEW.key = 'upnp_res_url')) EXECUTE FUNCTION upnp_catalog_changed();

INSERT INTO schema_version (version, name) VALUES (66,'upnp_catalog_revision') ON CONFLICT(version) DO NOTHING;

COMMIT;
