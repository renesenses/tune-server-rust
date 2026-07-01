use tracing::info;

use super::sqlite::SqliteDb;

struct Migration {
    version: i32,
    name: &'static str,
    up: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        up: "", // V1 is the CORE_SCHEMA applied by init_schema()
    },
    Migration {
        version: 2,
        name: "add_radio_stations",
        up: "
CREATE TABLE IF NOT EXISTS radio_stations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    homepage TEXT,
    logo_url TEXT,
    country TEXT,
    language TEXT,
    genre TEXT,
    codec TEXT,
    bitrate INTEGER,
    is_favorite INTEGER DEFAULT 0,
    last_played TEXT,
    play_count INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_radio_stations_favorite ON radio_stations(is_favorite);
",
    },
    Migration {
        version: 3,
        name: "add_listen_history",
        up: "
CREATE TABLE IF NOT EXISTS listen_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER REFERENCES tracks(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    artist_name TEXT,
    album_title TEXT,
    source TEXT DEFAULT 'local',
    duration_ms INTEGER DEFAULT 0,
    listened_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    zone_id INTEGER REFERENCES zones(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_listen_history_listened_at ON listen_history(listened_at);
CREATE INDEX IF NOT EXISTS idx_listen_history_track_id ON listen_history(track_id);
",
    },
    Migration {
        version: 4,
        name: "add_settings_table",
        up: "
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
",
    },
    Migration {
        version: 5,
        name: "add_bookmarks",
        up: "
CREATE TABLE IF NOT EXISTS bookmarks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER REFERENCES tracks(id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    label TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_bookmarks_track_id ON bookmarks(track_id);
",
    },
    Migration {
        version: 6,
        name: "add_profiles_favorites_tags_ratings",
        up: "
CREATE TABLE IF NOT EXISTS profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    avatar_path TEXT,
    password_hash TEXT,
    is_admin INTEGER DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS favorites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id INTEGER NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(profile_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_favorites_profile ON favorites(profile_id, item_type);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    color TEXT DEFAULT '#808080'
);

CREATE TABLE IF NOT EXISTS item_tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    UNIQUE(tag_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_item_tags_item ON item_tags(item_type, item_id);

CREATE TABLE IF NOT EXISTS album_ratings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    profile_id INTEGER NOT NULL DEFAULT 1,
    rating INTEGER NOT NULL CHECK(rating >= 1 AND rating <= 5),
    note TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(album_id, profile_id)
);
CREATE INDEX IF NOT EXISTS idx_album_ratings_album ON album_ratings(album_id);

CREATE TABLE IF NOT EXISTS smart_playlists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    sort_by TEXT DEFAULT 'title',
    sort_order TEXT DEFAULT 'asc',
    max_tracks INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

INSERT OR IGNORE INTO profiles (id, username, display_name, is_admin) VALUES (1, 'default', 'Default', 1);
",
    },
    Migration {
        version: 7,
        name: "add_alarms_network_mounts_podcasts",
        up: "
CREATE TABLE IF NOT EXISTS alarms (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_id INTEGER REFERENCES zones(id) ON DELETE CASCADE,
    time TEXT NOT NULL,
    enabled INTEGER DEFAULT 1,
    days TEXT DEFAULT '1,2,3,4,5,6,7',
    source_type TEXT DEFAULT 'playlist',
    source_id INTEGER,
    volume REAL DEFAULT 0.3,
    fade_in_seconds INTEGER DEFAULT 30,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS network_mounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mount_type TEXT NOT NULL DEFAULT 'smb',
    server TEXT NOT NULL,
    share TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    username TEXT,
    password TEXT,
    active INTEGER DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS podcast_subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    author TEXT,
    image_url TEXT,
    description TEXT,
    last_checked TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
",
    },
    Migration {
        version: 8,
        name: "add_radio_favorites_and_alarm_extras",
        // radio_favorites table is safe (IF NOT EXISTS); alarm columns are applied
        // programmatically via add_column_if_missing to survive re-runs on DBs
        // where the columns were already added by a previous partial migration.
        up: "
CREATE TABLE IF NOT EXISTS radio_favorites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    artist TEXT DEFAULT '',
    station_name TEXT DEFAULT '',
    cover_url TEXT,
    stream_url TEXT,
    saved_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(title, artist)
);
",
    },
    Migration {
        version: 9,
        name: "add_track_credits",
        up: "
CREATE TABLE IF NOT EXISTS track_credits (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL,
    artist_id INTEGER,
    artist_name TEXT NOT NULL,
    role TEXT DEFAULT 'performer',
    instrument TEXT,
    position INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_track_credits_track ON track_credits(track_id);
CREATE INDEX IF NOT EXISTS idx_track_credits_artist ON track_credits(artist_name);
",
    },
    Migration {
        version: 10,
        name: "add_album_artist_to_tracks",
        up: "", // Column included in CORE_SCHEMA; for existing DBs, applied programmatically
    },
    Migration {
        version: 11,
        name: "add_genres_column",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 12,
        name: "enhance_fts5_multi_column",
        up: "", // Applied programmatically to rebuild FTS with extra columns
    },
    Migration {
        version: 13,
        name: "add_offline_cache",
        up: "
CREATE TABLE IF NOT EXISTS offline_cache (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    track_title TEXT,
    artist_name TEXT,
    album_title TEXT,
    file_path TEXT,
    file_size INTEGER,
    duration_ms INTEGER,
    quality TEXT,
    downloaded_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME,
    status TEXT DEFAULT 'pending',
    error TEXT,
    UNIQUE(source, source_id)
);
CREATE INDEX IF NOT EXISTS idx_offline_cache_source ON offline_cache(source, source_id);
CREATE INDEX IF NOT EXISTS idx_offline_cache_status ON offline_cache(status);
",
    },
    Migration {
        version: 14,
        name: "add_sync_links",
        up: "
CREATE TABLE IF NOT EXISTS sync_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    local_playlist_id INTEGER NOT NULL,
    service TEXT NOT NULL,
    remote_playlist_id TEXT NOT NULL,
    direction TEXT NOT NULL DEFAULT '\"bidirectional\"',
    last_synced TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS sync_link_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_link_id INTEGER NOT NULL REFERENCES sync_links(id) ON DELETE CASCADE,
    side TEXT NOT NULL,
    tracks_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sync_snapshots_link ON sync_link_snapshots(playlist_link_id, side);
",
    },
    Migration {
        version: 15,
        name: "add_smart_collections",
        up: "
CREATE TABLE IF NOT EXISTS smart_collections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    rules TEXT NOT NULL DEFAULT '[]',
    match_mode TEXT NOT NULL DEFAULT '\"all\"',
    sort_by TEXT,
    sort_order TEXT NOT NULL DEFAULT '\"asc\"',
    max_limit INTEGER,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
",
    },
    Migration {
        version: 16,
        name: "add_performance_indexes",
        up: "
CREATE INDEX IF NOT EXISTS idx_artists_name ON artists(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_albums_title ON albums(title COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_albums_title_artist ON albums(title, artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_album_disc_track ON tracks(album_id, disc_number, track_number);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_title ON tracks(artist_id, title COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_tracks_source_path ON tracks(source, file_path);
CREATE INDEX IF NOT EXISTS idx_listen_history_zone ON listen_history(zone_id);
CREATE INDEX IF NOT EXISTS idx_listen_history_artist ON listen_history(artist_name);
CREATE INDEX IF NOT EXISTS idx_listen_history_album ON listen_history(album_title, artist_name);
CREATE INDEX IF NOT EXISTS idx_listen_history_track ON listen_history(title, artist_name);
CREATE INDEX IF NOT EXISTS idx_playlist_tracks_track ON playlist_tracks(track_id);
",
    },
    Migration {
        version: 17,
        name: "add_zone_gapless_enabled",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 18,
        name: "add_zone_group_and_sync_delay",
        up: "",
    },
    Migration {
        version: 19,
        name: "seed_default_smart_playlists",
        up: "
INSERT OR IGNORE INTO smart_playlists (name, rules, sort_by, sort_order, max_tracks)
    SELECT '50 Random Tracks', '[]', 'random', 'asc', 50
    WHERE NOT EXISTS (SELECT 1 FROM smart_playlists WHERE name = '50 Random Tracks');
INSERT OR IGNORE INTO smart_playlists (name, rules, sort_by, sort_order, max_tracks)
    SELECT 'Recently Added', '[]', 'added_at', 'desc', 100
    WHERE NOT EXISTS (SELECT 1 FROM smart_playlists WHERE name = 'Recently Added');
INSERT OR IGNORE INTO smart_playlists (name, rules, sort_by, sort_order, max_tracks)
    SELECT 'Most Played', '[]', 'play_count', 'desc', 50
    WHERE NOT EXISTS (SELECT 1 FROM smart_playlists WHERE name = 'Most Played');
INSERT OR IGNORE INTO smart_playlists (name, rules, sort_by, sort_order, max_tracks)
    SELECT 'Never Played', '[{\"field\":\"play_count\",\"op\":\"eq\",\"value\":\"0\"}]', 'title', 'asc', 100
    WHERE NOT EXISTS (SELECT 1 FROM smart_playlists WHERE name = 'Never Played');
",
    },
    Migration {
        version: 20,
        name: "add_waveform_column",
        up: "",
    },
    Migration {
        version: 21,
        name: "add_acoustid_columns",
        up: "",
    },
    Migration {
        version: 22,
        name: "add_track_source_links",
        up: "
CREATE TABLE IF NOT EXISTS track_source_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    service_track_id TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.0,
    match_method TEXT,
    linked_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(track_id, service)
);
CREATE INDEX IF NOT EXISTS idx_track_source_links_track ON track_source_links(track_id);
CREATE INDEX IF NOT EXISTS idx_track_source_links_service ON track_source_links(service);
",
    },
    Migration {
        version: 23,
        name: "add_trailing_silence",
        up: "",
    },
    Migration {
        version: 24,
        name: "add_synced_lyrics",
        up: "",
    },
    Migration {
        version: 25,
        name: "add_zone_dsp",
        up: "",
    },
    Migration {
        version: 26,
        name: "add_zone_playback_position",
        up: "",
    },
    Migration {
        version: 27,
        name: "add_zone_max_sample_rate",
        up: "",
    },
    Migration {
        version: 28,
        name: "add_profile_email_and_argon2_password",
        up: "",
    },
    Migration {
        version: 29,
        name: "add_smart_collections_extra_columns",
        up: "",
    },
    Migration {
        version: 30,
        name: "add_track_comments",
        up: "",
    },
    Migration {
        version: 31,
        name: "add_streaming_queue_source",
        up: "",
    },
    Migration {
        version: 32,
        name: "add_listen_history_cover_url",
        up: "",
    },
    Migration {
        version: 33,
        name: "seed_default_radios",
        up: "
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP', 'https://icecast.radiofrance.fr/fip-hifi.aac', 'Éclectique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Rock', 'https://icecast.radiofrance.fr/fiprock-hifi.aac', 'Rock', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Jazz', 'https://icecast.radiofrance.fr/fipjazz-hifi.aac', 'Jazz', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Groove', 'https://icecast.radiofrance.fr/fipgroove-hifi.aac', 'Groove', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Pop', 'https://icecast.radiofrance.fr/fippop-hifi.aac', 'Pop', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Electro', 'https://icecast.radiofrance.fr/fipelectro-hifi.aac', 'Électronique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Monde', 'https://icecast.radiofrance.fr/fipworld-hifi.aac', 'Monde', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Reggae', 'https://icecast.radiofrance.fr/fipreggae-hifi.aac', 'Reggae', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Nouveautés', 'https://icecast.radiofrance.fr/fipnouveautes-hifi.aac', 'Éclectique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Metal', 'https://icecast.radiofrance.fr/fipmetal-hifi.aac', 'Metal', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Hip-Hop', 'https://icecast.radiofrance.fr/fiphiphop-hifi.aac', 'Hip-Hop', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Sacré français', 'https://icecast.radiofrance.fr/fipsacrefrancais-hifi.aac', 'Chanson française', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Latino', 'https://icecast.radiofrance.fr/fiplatino-hifi.aac', 'Latino', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('FIP Tout nouveau', 'https://icecast.radiofrance.fr/fiptoutnouveautoutchaud-hifi.aac', 'Éclectique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique', 'https://icecast.radiofrance.fr/francemusique-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Classique Easy', 'https://icecast.radiofrance.fr/francemusiqueeasyclassique-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Classique Plus', 'https://icecast.radiofrance.fr/francemusiqueclassiqueplus-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Concerts', 'https://icecast.radiofrance.fr/francemusiqueconcertsradiofrance-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Jazz', 'https://icecast.radiofrance.fr/francemusiquelajazz-hifi.aac', 'Jazz', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Contemporaine', 'https://icecast.radiofrance.fr/francemusiquelacontemporaine-hifi.aac', 'Contemporaine', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Baroque', 'https://icecast.radiofrance.fr/francemusiquebaroque-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Opéra', 'https://icecast.radiofrance.fr/francemusiqueopera-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Musiques du monde', 'https://icecast.radiofrance.fr/francemusiqueocoramondial-hifi.aac', 'Monde', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Culture', 'https://icecast.radiofrance.fr/franceculture-hifi.aac', 'Culture', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Inter', 'https://icecast.radiofrance.fr/franceinter-hifi.aac', 'Généraliste', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('Mouv''', 'https://icecast.radiofrance.fr/mouv-hifi.aac', 'Hip-Hop', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('Mouv'' Xtra', 'https://icecast.radiofrance.fr/mouvxtra-hifi.aac', 'Hip-Hop', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('Radio Classique', 'https://radioclassique.ice.infomaniak.ch/radioclassique-high.mp3', 'Classique', 'France');
",
    },
    Migration {
        version: 34,
        name: "add_track_metadata_table",
        up: "
CREATE TABLE IF NOT EXISTS track_metadata (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (track_id, key)
);
CREATE INDEX IF NOT EXISTS idx_track_metadata_key ON track_metadata(key);
",
    },
    Migration {
        version: 35,
        name: "add_zone_fixed_volume",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 36,
        name: "add_zone_autoplay_enabled",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 37,
        name: "add_listen_history_source_id_album_id",
        up: "",
    },
    Migration {
        version: 38,
        name: "add_zones_is_hidden",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 39,
        name: "add_zones_last_play_state",
        up: "", // Applied programmatically via ensure_zones_is_hidden (idempotent ALTER)
    },
    Migration {
        version: 40,
        name: "add_zones_dsd_mode",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 41,
        name: "seed_default_smart_collections",
        up: "
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '💎 Audiophile', '[{\"field\":\"sample_rate\",\"operator\":\"greater_than\",\"value\":\"96000\"}]', 'all', '💎', '#9B59B6', 'Enregistrements haute résolution' WHERE NOT EXISTS (SELECT 1 FROM smart_collections);
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎬 Bandes Originales', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"soundtrack\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"Stage\"}]', 'any', '🎬', '#C0392B', 'Bandes originales de films' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Bandes Originales%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎻 Classique', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"classical\"}]', 'all', '🎻', '#6B6ED9', 'Musique classique et orchestrale' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Classique%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎧 Electro & Ambient', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"electro\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"ambient\"}]', 'any', '🎧', '#00CED1', 'Électronique et ambient' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Electro%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🇫🇷 French Touch', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"chanson\"}]', 'all', '🇫🇷', '#2060B8', 'Chanson française' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%French%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎷 Jazz', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"jazz\"}]', 'all', '🎷', '#E8A838', 'Tous les albums de jazz' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Jazz%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎸 Rock', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"rock\"}]', 'all', '🎸', '#E04040', 'Rock, alt-rock, prog-rock' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Rock%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '💿 SACD / DSD', '[{\"field\":\"format\",\"operator\":\"equals\",\"value\":\"dsd\"}]', 'all', '💿', '#C0C0C0', 'Super Audio CD et DSD' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%DSD%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🕺 Soul & Funk', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"soul\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"funk\"}]', 'any', '🕺', '#E67E22', 'Soul, Funk, R&B' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Soul%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🆕 Récents', '[{\"field\":\"added_at\",\"operator\":\"greater_than\",\"value\":\"90d\"}]', 'all', '🆕', '#27AE60', 'Ajoutés dans les 90 derniers jours' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%cent%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🖼️ Sans pochette', '[{\"field\":\"format\",\"operator\":\"is_not_empty\",\"value\":\"\"}]', 'all', '🖼️', '#7F8C8D', 'Albums sans couverture' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%pochette%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎹 Piano', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"piano\"}]', 'all', '🎹', '#8E44AD', 'Piano solo et concertos' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Piano%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎤 Vocal / A cappella', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"vocal\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"cappella\"}]', 'any', '🎤', '#D35400', 'Musique vocale et a cappella' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Vocal%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎵 Blues', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"blues\"}]', 'all', '🎵', '#2C3E50', 'Blues et blues-rock' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Blues%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🌍 World Music', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"world\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"ethnic\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"folk\"}]', 'any', '🌍', '#16A085', 'Musiques du monde et folk' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%World%');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) SELECT '🎺 Pop', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"pop\"}]', 'all', '🎺', '#E91E63', 'Pop et synth-pop' WHERE NOT EXISTS (SELECT 1 FROM smart_collections WHERE name LIKE '%Pop%');
",
    },
    Migration {
        version: 42,
        name: "create_sync_changelog",
        up: "
CREATE TABLE IF NOT EXISTS sync_changelog (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_type TEXT NOT NULL,
    entity_id INTEGER NOT NULL,
    action TEXT NOT NULL,
    changed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    synced INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sync_changelog_unsynced ON sync_changelog(synced, changed_at);
CREATE INDEX IF NOT EXISTS idx_sync_changelog_entity ON sync_changelog(entity_type, entity_id);
",
    },
    Migration {
        version: 43,
        name: "add_lyrics_cache",
        up: "
CREATE TABLE IF NOT EXISTS lyrics_cache (
    track_id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    synced_lyrics TEXT,
    plain_lyrics TEXT,
    source TEXT NOT NULL DEFAULT 'lrclib',
    fetched_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
",
    },
    Migration {
        version: 44,
        name: "add_advanced_alarm_columns",
        up: "",
    },
    Migration {
        version: 45,
        name: "add_profile_id_to_history_and_ratings",
        up: "",
    },
    Migration {
        version: 46,
        name: "autoplay_default_off",
        up: "UPDATE zones SET autoplay_enabled = 0 WHERE autoplay_enabled = 1;",
    },
    Migration {
        version: 47,
        name: "reseed_smart_collections",
        up: "
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('💎 Audiophile', '[{\"field\":\"sample_rate\",\"operator\":\"greater_than\",\"value\":\"96000\"}]', 'all', '💎', '#9B59B6', 'Enregistrements haute résolution');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎬 Bandes Originales', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"soundtrack\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"Stage\"}]', 'any', '🎬', '#C0392B', 'Bandes originales de films');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎻 Classique', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"classical\"}]', 'all', '🎻', '#6B6ED9', 'Musique classique et orchestrale');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎧 Electro & Ambient', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"electro\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"ambient\"}]', 'any', '🎧', '#00CED1', 'Électronique et ambient');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🇫🇷 French Touch', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"chanson\"}]', 'all', '🇫🇷', '#2060B8', 'Chanson française');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎷 Jazz', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"jazz\"}]', 'all', '🎷', '#E8A838', 'Tous les albums de jazz');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎸 Rock', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"rock\"}]', 'all', '🎸', '#E04040', 'Rock, alt-rock, prog-rock');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('💿 SACD / DSD', '[{\"field\":\"format\",\"operator\":\"equals\",\"value\":\"dsd\"}]', 'all', '💿', '#C0C0C0', 'Super Audio CD et DSD');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🕺 Soul & Funk', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"soul\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"funk\"}]', 'any', '🕺', '#E67E22', 'Soul, Funk, R&B');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🆕 Récents', '[{\"field\":\"added_at\",\"operator\":\"greater_than\",\"value\":\"90d\"}]', 'all', '🆕', '#27AE60', 'Ajoutés dans les 90 derniers jours');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎹 Piano', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"piano\"}]', 'all', '🎹', '#8E44AD', 'Piano solo et concertos');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎤 Vocal / A cappella', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"vocal\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"cappella\"}]', 'any', '🎤', '#D35400', 'Musique vocale et a cappella');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎵 Blues', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"blues\"}]', 'all', '🎵', '#2C3E50', 'Blues et blues-rock');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🌍 World Music', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"world\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"ethnic\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"folk\"}]', 'any', '🌍', '#16A085', 'Musiques du monde et folk');
INSERT OR IGNORE INTO smart_collections (name, rules, match_mode, icon, color, description) VALUES ('🎺 Pop', '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"pop\"}]', 'all', '🎺', '#E91E63', 'Pop et synth-pop');
",
    },
    Migration {
        version: 48,
        name: "smart_playlists_match_mode",
        up: "ALTER TABLE smart_playlists ADD COLUMN match_mode TEXT NOT NULL DEFAULT 'all';",
    },
    Migration {
        version: 49,
        name: "unified_queue_items",
        // Applied programmatically (create table + one-time copy) via
        // migrate_to_unified_queue so it is idempotent across re-runs and
        // tolerant of the lazily-created streaming_queue table.
        up: "",
    },
];

/// v0.9 rc.2 — one-time copy of the split `play_queue` / `streaming_queue`
/// tables into the unified `queue_items` table. Idempotent: copies only when
/// `queue_items` is empty, so re-runs never duplicate. Tolerant of a missing
/// `streaming_queue` table (it is lazily created by the repo). Created without
/// FK constraints so orphaned rows migrate cleanly; the fresh CORE_SCHEMA
/// version carries the FKs.
fn migrate_to_unified_queue(db: &SqliteDb) {
    let conn = db.connection().lock().unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS queue_items (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            zone_id INTEGER NOT NULL,
            position INTEGER NOT NULL DEFAULT 0,
            is_current INTEGER DEFAULT 0,
            track_id INTEGER,
            source TEXT,
            source_id TEXT,
            title TEXT,
            artist TEXT,
            album TEXT,
            cover_url TEXT,
            duration_ms INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_queue_items_zone_id ON queue_items(zone_id);",
    )
    .ok();

    // Only copy once — when the unified table has no rows yet.
    let already: i64 = conn
        .query_row("SELECT COUNT(*) FROM queue_items", [], |r| r.get(0))
        .unwrap_or(0);
    if already > 0 {
        return;
    }

    // Local rows: keep track_id, tag source='local'. Display fields stay NULL
    // (joined from tracks at read time, as before).
    conn.execute_batch(
        "INSERT INTO queue_items (zone_id, position, is_current, track_id, source, duration_ms)
         SELECT zone_id, position, is_current, track_id, 'local', 0 FROM play_queue;",
    )
    .ok();

    // Streaming rows: inline metadata. Tolerant if streaming_queue is absent.
    conn.execute_batch(
        "INSERT INTO queue_items (zone_id, position, is_current, source, source_id, title, artist, album, cover_url, duration_ms)
         SELECT zone_id, position, 0, source, source_id, title, artist, album, cover_url, duration_ms FROM streaming_queue;",
    )
    .ok();

    // Data is now in queue_items — drop the legacy split tables. This runs only
    // on the one-time copy pass (the early return above skips it afterwards),
    // so the drop always immediately follows a successful copy.
    conn.execute_batch("DROP TABLE IF EXISTS play_queue; DROP TABLE IF EXISTS streaming_queue;")
        .ok();
}

fn add_column_if_missing(db: &SqliteDb, table: &str, column: &str, col_type: &str) {
    let conn = db.connection().lock().unwrap();
    let has_column = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names.iter().any(|name| name == column))
        })
        .unwrap_or(false);
    drop(conn);
    if !has_column {
        db.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {col_type};"
        ))
        .ok();
    }
}

/// Upgrade FTS5 tables from single-column (title only) to multi-column
/// (artist_name, genre, composer, etc.) for richer full-text search.
fn upgrade_fts5_tables(db: &SqliteDb) {
    let sql = "
        -- Drop old triggers
        DROP TRIGGER IF EXISTS tracks_fts_insert;
        DROP TRIGGER IF EXISTS tracks_fts_update;
        DROP TRIGGER IF EXISTS tracks_fts_delete;
        DROP TRIGGER IF EXISTS albums_fts_insert;
        DROP TRIGGER IF EXISTS albums_fts_update;
        DROP TRIGGER IF EXISTS albums_fts_delete;
        DROP TRIGGER IF EXISTS artists_fts_insert;
        DROP TRIGGER IF EXISTS artists_fts_update;
        DROP TRIGGER IF EXISTS artists_fts_delete;

        -- Drop old FTS tables
        DROP TABLE IF EXISTS tracks_fts;
        DROP TABLE IF EXISTS albums_fts;
        DROP TABLE IF EXISTS artists_fts;

        -- Recreate with multiple columns (contentless — triggers handle sync)
        CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
            title, artist_name, album_title, genre, composer,
            tokenize='unicode61 remove_diacritics 2',
            content='', content_rowid='id'
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS albums_fts USING fts5(
            title, artist_name, genre,
            tokenize='unicode61 remove_diacritics 2',
            content='', content_rowid='id'
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS artists_fts USING fts5(
            name, sort_name,
            tokenize='unicode61 remove_diacritics 2',
            content='', content_rowid='id'
        );

        -- Auto-sync triggers: tracks
        CREATE TRIGGER IF NOT EXISTS tracks_fts_insert AFTER INSERT ON tracks BEGIN
            INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer)
            VALUES (new.id, new.title,
                    (SELECT name FROM artists WHERE id = new.artist_id),
                    (SELECT title FROM albums WHERE id = new.album_id),
                    new.genre, new.composer);
        END;
        CREATE TRIGGER IF NOT EXISTS tracks_fts_update AFTER UPDATE ON tracks BEGIN
            INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_title, genre, composer)
            VALUES ('delete', old.id, old.title,
                    (SELECT name FROM artists WHERE id = old.artist_id),
                    (SELECT title FROM albums WHERE id = old.album_id),
                    old.genre, old.composer);
            INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer)
            VALUES (new.id, new.title,
                    (SELECT name FROM artists WHERE id = new.artist_id),
                    (SELECT title FROM albums WHERE id = new.album_id),
                    new.genre, new.composer);
        END;
        CREATE TRIGGER IF NOT EXISTS tracks_fts_delete AFTER DELETE ON tracks BEGIN
            INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_title, genre, composer)
            VALUES ('delete', old.id, old.title,
                    (SELECT name FROM artists WHERE id = old.artist_id),
                    (SELECT title FROM albums WHERE id = old.album_id),
                    old.genre, old.composer);
        END;

        -- Auto-sync triggers: albums
        CREATE TRIGGER IF NOT EXISTS albums_fts_insert AFTER INSERT ON albums BEGIN
            INSERT INTO albums_fts(rowid, title, artist_name, genre)
            VALUES (new.id, new.title,
                    (SELECT name FROM artists WHERE id = new.artist_id),
                    new.genre);
        END;
        CREATE TRIGGER IF NOT EXISTS albums_fts_update AFTER UPDATE ON albums BEGIN
            INSERT INTO albums_fts(albums_fts, rowid, title, artist_name, genre)
            VALUES ('delete', old.id, old.title,
                    (SELECT name FROM artists WHERE id = old.artist_id),
                    old.genre);
            INSERT INTO albums_fts(rowid, title, artist_name, genre)
            VALUES (new.id, new.title,
                    (SELECT name FROM artists WHERE id = new.artist_id),
                    new.genre);
        END;
        CREATE TRIGGER IF NOT EXISTS albums_fts_delete AFTER DELETE ON albums BEGIN
            INSERT INTO albums_fts(albums_fts, rowid, title, artist_name, genre)
            VALUES ('delete', old.id, old.title,
                    (SELECT name FROM artists WHERE id = old.artist_id),
                    old.genre);
        END;

        -- Auto-sync triggers: artists
        CREATE TRIGGER IF NOT EXISTS artists_fts_insert AFTER INSERT ON artists BEGIN
            INSERT INTO artists_fts(rowid, name, sort_name) VALUES (new.id, new.name, new.sort_name);
        END;
        CREATE TRIGGER IF NOT EXISTS artists_fts_update AFTER UPDATE ON artists BEGIN
            INSERT INTO artists_fts(artists_fts, rowid, name, sort_name) VALUES ('delete', old.id, old.name, old.sort_name);
            INSERT INTO artists_fts(rowid, name, sort_name) VALUES (new.id, new.name, new.sort_name);
        END;
        CREATE TRIGGER IF NOT EXISTS artists_fts_delete AFTER DELETE ON artists BEGIN
            INSERT INTO artists_fts(artists_fts, rowid, name, sort_name) VALUES ('delete', old.id, old.name, old.sort_name);
        END;
    ";

    if let Err(e) = db.execute_batch(sql) {
        tracing::warn!(error = %e, "fts5_upgrade_failed");
        return;
    }
    info!("fts5_tables_upgraded_to_multi_column");

    let populate = "
        INSERT OR IGNORE INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer)
        SELECT t.id, t.title,
               (SELECT name FROM artists WHERE id = t.artist_id),
               (SELECT title FROM albums WHERE id = t.album_id),
               t.genre, t.composer
        FROM tracks t;
        INSERT OR IGNORE INTO albums_fts(rowid, title, artist_name, genre)
        SELECT a.id, a.title,
               (SELECT name FROM artists WHERE id = a.artist_id),
               a.genre
        FROM albums a;
        INSERT OR IGNORE INTO artists_fts(rowid, name, sort_name)
        SELECT id, name, sort_name FROM artists;
    ";
    if let Err(e) = db.execute_batch(populate) {
        tracing::warn!(error = %e, "fts5_populate_failed");
    } else {
        info!("fts5_tables_populated");
    }
}

pub fn run_migrations(db: &SqliteDb) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        )",
    )?;

    let current_version = {
        let conn = db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _migrations",
            [],
            |row| row.get::<_, i32>(0),
        )
        .map_err(|e| e.to_string())?
    };

    let tables_exist = {
        let conn = db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='artists'",
            [],
            |row| row.get::<_, i32>(0),
        )
        .map_err(|e| e.to_string())?
            > 0
    };

    if tables_exist && current_version == 0 {
        db.execute(
            "INSERT OR IGNORE INTO _migrations (version, name) VALUES (?, ?)",
            &[&1i32 as &dyn rusqlite::types::ToSql, &"initial_schema"],
        )?;
        info!(version = 1, "migration_marked_existing");
    }

    for migration in MIGRATIONS {
        if migration.version <= current_version.max(if tables_exist { 1 } else { 0 }) {
            continue;
        }

        info!(
            version = migration.version,
            name = migration.name,
            "migration_applying"
        );

        if !migration.up.is_empty() {
            db.execute_batch(migration.up)?;
        }

        // Programmatic migrations for column additions (safe if column already exists)
        if migration.version == 8 {
            // These were originally bare ALTER TABLE statements that would crash
            // on re-run if the columns already existed (e.g. partial migration).
            add_column_if_missing(db, "alarms", "name", "TEXT DEFAULT 'Alarm'");
            add_column_if_missing(db, "alarms", "one_shot", "INTEGER DEFAULT 0");
            add_column_if_missing(db, "alarms", "skip_holidays", "INTEGER DEFAULT 0");
            add_column_if_missing(db, "alarms", "source_name", "TEXT");
            add_column_if_missing(db, "alarms", "fade_duration_s", "INTEGER DEFAULT 60");
            add_column_if_missing(db, "alarms", "last_fired_at", "DATETIME");
        }
        if migration.version == 10 {
            add_column_if_missing(db, "tracks", "album_artist", "TEXT");
        }
        if migration.version == 11 {
            add_column_if_missing(db, "albums", "genres", "TEXT");
            add_column_if_missing(db, "tracks", "genres", "TEXT");
        }
        if migration.version == 12 {
            upgrade_fts5_tables(db);
        }
        if migration.version == 17 {
            add_column_if_missing(db, "zones", "gapless_enabled", "INTEGER DEFAULT 1");
        }
        if migration.version == 18 {
            add_column_if_missing(db, "zones", "group_id", "TEXT");
            add_column_if_missing(db, "zones", "sync_delay_ms", "INTEGER NOT NULL DEFAULT 0");
        }
        if migration.version == 20 {
            add_column_if_missing(db, "tracks", "waveform_json", "TEXT");
        }
        if migration.version == 21 {
            add_column_if_missing(db, "tracks", "acoustid_fingerprint", "TEXT");
            add_column_if_missing(db, "tracks", "acoustid_confidence", "REAL");
        }
        if migration.version == 23 {
            add_column_if_missing(db, "tracks", "trailing_silence_ms", "INTEGER");
        }
        if migration.version == 24 {
            add_column_if_missing(db, "tracks", "synced_lyrics", "TEXT");
        }
        if migration.version == 25 {
            add_column_if_missing(db, "zones", "dsp_preset_id", "INTEGER");
            add_column_if_missing(db, "zones", "dsp_enabled", "INTEGER DEFAULT 0");
        }
        if migration.version == 26 {
            add_column_if_missing(
                db,
                "zones",
                "last_position_ms",
                "INTEGER NOT NULL DEFAULT 0",
            );
            add_column_if_missing(db, "zones", "last_track_id", "INTEGER");
            add_column_if_missing(db, "zones", "last_track_source", "TEXT");
            add_column_if_missing(db, "zones", "last_track_source_id", "TEXT");
        }
        if migration.version == 27 {
            add_column_if_missing(db, "zones", "max_sample_rate", "INTEGER");
        }
        if migration.version == 28 {
            add_column_if_missing(db, "profiles", "email", "TEXT");
            add_column_if_missing(db, "profiles", "password_hash_v2", "TEXT");
        }
        if migration.version == 29 {
            add_column_if_missing(db, "smart_collections", "description", "TEXT");
            add_column_if_missing(db, "smart_collections", "icon", "TEXT");
            add_column_if_missing(db, "smart_collections", "color", "TEXT");
        }
        if migration.version == 30 {
            add_column_if_missing(db, "tracks", "comments", "TEXT");
        }
        if migration.version == 31 {
            add_column_if_missing(db, "streaming_queue", "source", "TEXT");
        }
        if migration.version == 32 {
            add_column_if_missing(db, "listen_history", "cover_url", "TEXT");
        }
        if migration.version == 35 {
            add_column_if_missing(db, "zones", "fixed_volume", "INTEGER DEFAULT 0");
        }
        if migration.version == 36 {
            add_column_if_missing(db, "zones", "autoplay_enabled", "INTEGER DEFAULT 0");
        }
        if migration.version == 37 {
            add_column_if_missing(db, "listen_history", "source_id", "TEXT");
            add_column_if_missing(db, "listen_history", "album_id", "INTEGER");
        }
        if migration.version == 38 {
            add_column_if_missing(db, "zones", "is_hidden", "INTEGER DEFAULT 0");
        }
        if migration.version == 39 {
            add_column_if_missing(db, "zones", "last_play_state", "TEXT DEFAULT 'stopped'");
        }
        if migration.version == 40 {
            add_column_if_missing(db, "zones", "dsd_mode", "TEXT DEFAULT 'auto'");
        }
        if migration.version == 44 {
            add_column_if_missing(db, "alarms", "days_of_week", "TEXT DEFAULT '1111111'");
            add_column_if_missing(db, "alarms", "multi_zone_ids", "TEXT");
        }
        if migration.version == 45 {
            add_column_if_missing(db, "listen_history", "profile_id", "INTEGER");
        }

        db.execute(
            "INSERT INTO _migrations (version, name) VALUES (?, ?)",
            &[
                &migration.version as &dyn rusqlite::types::ToSql,
                &migration.name,
            ],
        )?;

        info!(
            version = migration.version,
            name = migration.name,
            "migration_applied"
        );
    }

    // Post-migration safety pass: ensure critical columns always exist regardless
    // of what migration version the DB came from. This guards against:
    //  - DBs where migrations were partially applied (e.g. power loss mid-migration)
    //  - DBs migrated from very old versions that skipped intermediate steps
    //  - Any discrepancy between CORE_SCHEMA and programmatic migration columns
    add_column_if_missing(db, "zones", "gapless_enabled", "INTEGER DEFAULT 1");
    add_column_if_missing(db, "zones", "group_id", "TEXT");
    add_column_if_missing(db, "zones", "sync_delay_ms", "INTEGER NOT NULL DEFAULT 0");
    add_column_if_missing(
        db,
        "zones",
        "last_position_ms",
        "INTEGER NOT NULL DEFAULT 0",
    );
    add_column_if_missing(db, "zones", "last_track_id", "INTEGER");
    add_column_if_missing(db, "zones", "last_track_source", "TEXT");
    add_column_if_missing(db, "zones", "last_track_source_id", "TEXT");
    add_column_if_missing(db, "zones", "max_sample_rate", "INTEGER");
    add_column_if_missing(db, "zones", "dsp_preset_id", "INTEGER");
    add_column_if_missing(db, "zones", "dsp_enabled", "INTEGER DEFAULT 0");
    add_column_if_missing(db, "zones", "fixed_volume", "INTEGER DEFAULT 0");
    add_column_if_missing(db, "zones", "autoplay_enabled", "INTEGER DEFAULT 0");
    add_column_if_missing(db, "zones", "dsd_mode", "TEXT DEFAULT 'auto'");

    add_column_if_missing(db, "listen_history", "source_id", "TEXT");
    add_column_if_missing(db, "listen_history", "album_id", "INTEGER");
    add_column_if_missing(db, "listen_history", "profile_id", "INTEGER");

    add_column_if_missing(db, "alarms", "days_of_week", "TEXT DEFAULT '1111111'");
    add_column_if_missing(db, "alarms", "multi_zone_ids", "TEXT");

    // v0.9 rc.2 — unify play_queue + streaming_queue into queue_items (v49).
    migrate_to_unified_queue(db);

    db.execute_batch("ANALYZE;").ok();
    info!("sqlite_analyze_complete");

    Ok(())
}

pub fn current_version(db: &SqliteDb) -> Result<i32, String> {
    let has_table = {
        let conn = db.connection().lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_migrations'",
            [],
            |row| row.get::<_, i32>(0),
        )
        .map_err(|e| e.to_string())?
            > 0
    };

    if !has_table {
        return Ok(0);
    }

    let conn = db.connection().lock().unwrap();
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _migrations",
        [],
        |row| row.get::<_, i32>(0),
    )
    .map_err(|e| e.to_string())
}

pub fn latest_version() -> i32 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

// ─── PostgreSQL migration runner ─────────────────────────────────────

/// Embedded PG migration scripts. Each tuple is (version, name, sql).
/// The SQL files are compiled into the binary so no filesystem access
/// is needed at runtime.
#[cfg(feature = "postgres")]
const PG_MIGRATIONS: &[(i32, &str, &str)] = &[
    (
        1,
        "initial_schema",
        include_str!("../../migrations/postgres/001_initial_schema.sql"),
    ),
    (
        2,
        "fts_tsvector",
        include_str!("../../migrations/postgres/002_fts_tsvector.sql"),
    ),
    (
        3,
        "track_metadata_columns",
        include_str!("../../migrations/postgres/003_track_metadata_columns.sql"),
    ),
    (
        4,
        "listen_history",
        include_str!("../../migrations/postgres/004_listen_history.sql"),
    ),
    (
        5,
        "additional_tables",
        include_str!("../../migrations/postgres/005_additional_tables.sql"),
    ),
    (
        6,
        "missing_columns",
        include_str!("../../migrations/postgres/006_missing_columns.sql"),
    ),
    (
        7,
        "podcast_subscriptions",
        include_str!("../../migrations/postgres/007_podcast_subscriptions.sql"),
    ),
];

/// Run all pending PostgreSQL migrations against the pool.
///
/// Uses a `schema_version` table (matching the convention in the SQL
/// files) to track which migrations have been applied.  Migrations
/// that wrap their body in `BEGIN; … COMMIT;` are executed as-is;
/// the runner does not add an outer transaction so that each script
/// controls its own transactional boundaries.
#[cfg(feature = "postgres")]
pub async fn run_pg_migrations(pool: &sqlx::PgPool) -> Result<(), String> {
    // Ensure the tracking table exists.  The 001 script creates
    // `schema_version`, but on a truly empty database we need it
    // before we can query it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TIMESTAMPTZ DEFAULT now(),
            name TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| format!("pg create schema_version: {e}"))?;

    // What has already been applied?
    let current: i32 =
        sqlx::query_scalar::<_, i32>("SELECT COALESCE(MAX(version), 0) FROM schema_version")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("pg read schema_version: {e}"))?;

    for &(version, name, sql) in PG_MIGRATIONS {
        if version <= current {
            continue;
        }

        info!(version, name, "pg_migration_applying");

        // Each migration file manages its own BEGIN/COMMIT, so we
        // execute the raw SQL directly.
        sqlx::raw_sql(sql)
            .execute(pool)
            .await
            .map_err(|e| format!("pg migration {version} ({name}): {e}"))?;

        info!(version, name, "pg_migration_applied");
    }

    // Run ANALYZE on key tables for the query planner.
    sqlx::raw_sql("ANALYZE artists; ANALYZE albums; ANALYZE tracks;")
        .execute(pool)
        .await
        .ok();
    info!("pg_analyze_complete");

    Ok(())
}

/// Latest PG migration version (for diagnostics).
#[cfg(feature = "postgres")]
pub fn pg_latest_version() -> i32 {
    PG_MIGRATIONS.last().map(|&(v, _, _)| v).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_runs_all_migrations() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        assert_eq!(current_version(&db).unwrap(), latest_version());

        let conn = db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(tables.contains(&"radio_stations".to_string()));
        assert!(tables.contains(&"listen_history".to_string()));
        assert!(tables.contains(&"settings".to_string()));
        assert!(tables.contains(&"bookmarks".to_string()));
    }

    #[test]
    fn migrations_are_idempotent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        run_migrations(&db).unwrap();
        assert_eq!(current_version(&db).unwrap(), latest_version());
    }

    #[test]
    fn migration_count_matches() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, latest_version());
    }
}
