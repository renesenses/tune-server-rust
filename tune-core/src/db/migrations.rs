use tracing::{info, warn};

use super::migration_status;
use super::sqlite::SqliteDb;

struct Migration {
    version: i32,
    name: &'static str,
    up: &'static str,
}

/// v0.9 — collapse the two per-source position spaces into ONE contiguous space
/// per zone. After the unified copy (v52), local rows sit at 0..L-1 and
/// streaming rows at 0..S-1 (overlapping); shift streaming rows up by the zone's
/// local count so the whole queue is one ordered sequence 0..L+S-1, which the
/// unified repo/orchestrator expect. Runs exactly once via version tracking, so
/// it can't double-shift an already-unified queue.
const RENUMBER_QUEUE_POSITIONS_SQL: &str = "UPDATE queue_items \
    SET position = position + ( \
        SELECT COUNT(*) FROM queue_items q2 \
        WHERE q2.zone_id = queue_items.zone_id AND q2.track_id IS NOT NULL \
    ) \
    WHERE track_id IS NULL;";

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
    -- `active` dit l'INTENTION de l'utilisateur (« ce partage doit etre
    -- monte »). Les trois colonnes ci-dessous disent le CONSTAT — ce qui s'est
    -- reellement passe au dernier essai. Rien ne les portait, et c'est ce qui
    -- rendait #1916 invisible : le partage restait affiche comme monte alors
    -- que le remontage au demarrage avait echoue, et la lecture rendait une
    -- erreur reseau qui ne nommait jamais la cause.
    smb_version TEXT,        -- dialecte retenu : 'negocie' | '2.0' | '1.0' (#1834)
    mount_state TEXT,        -- 'mounted' | 'failed' — NUL = jamais tente
    last_mount_error TEXT,   -- stderr de mount.cifs, jamais le mot de passe
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS podcast_subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    author TEXT,
    image_url TEXT,
    description TEXT,
    source_id TEXT,
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
-- 'FIP Latino' (fiplatino) and 'FIP Tout nouveau' (fiptoutnouveautoutchaud) used
-- to be seeded here and are deliberately gone: Radio France no longer serves
-- either slug. Both answer 404 while every other FIP webradio above answers 200
-- (checked 2026-08-08, and every plausible spelling — fiplatina, fipsalsa,
-- fiptoutnouveau — 404s too). They shipped as two stations that could never
-- play. Migration 70 removes them from databases that already seeded them.
-- Do not re-add without checking the URL first. Forum #626 (Jean Valjean).
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique', 'https://icecast.radiofrance.fr/francemusique-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Classique Easy', 'https://icecast.radiofrance.fr/francemusiqueeasyclassique-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Classique Plus', 'https://icecast.radiofrance.fr/francemusiqueclassiqueplus-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Concerts', 'https://icecast.radiofrance.fr/francemusiqueconcertsradiofrance-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Jazz', 'https://icecast.radiofrance.fr/francemusiquelajazz-hifi.aac', 'Jazz', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Contemporaine', 'https://icecast.radiofrance.fr/francemusiquelacontemporaine-hifi.aac', 'Contemporaine', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Baroque', 'https://icecast.radiofrance.fr/francemusiquebaroque-hifi.aac', 'Classique', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Musique Opéra', 'https://icecast.radiofrance.fr/francemusiqueopera-hifi.aac', 'Classique', 'France');
-- France Musique Musiques du monde (slug francemusiqueocoramondial) used to be
-- seeded here: Radio France answers 404 on that slug since at least
-- 2026-08-20, while the eight other France Musique webradios above answer 200
-- with content-type audio/aac. Migration 78 removes it from databases that
-- already seeded it.
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Culture', 'https://icecast.radiofrance.fr/franceculture-hifi.aac', 'Culture', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('France Inter', 'https://icecast.radiofrance.fr/franceinter-hifi.aac', 'Généraliste', 'France');
INSERT OR IGNORE INTO radio_stations (name, url, genre, country) VALUES ('Mouv''', 'https://icecast.radiofrance.fr/mouv-hifi.aac', 'Hip-Hop', 'France');
-- Mouv Xtra (slug mouvxtra) used to be seeded here, same verdict: 404 while
-- the Mouv entry just above answers 200. Migration 78 removes it too.
--
-- How to check before adding a station back, or a new one (issue #1960): fetch
-- the URL with redirects followed and look at BOTH the status and the
-- content-type. Anything but a 2xx with an audio-ish content-type is a station
-- that cannot play. A 200 answering text/html is the worst case: nothing errors
-- out and the listener just gets silence.
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
        // Migration 47 (reseed_smart_collections) re-inserted the 16 default
        // collections with a bare `INSERT OR IGNORE ... VALUES`, but the table
        // has no UNIQUE constraint on `name`, so OR IGNORE never fired and every
        // default ended up duplicated (twice, or more across version jumps).
        // Deduplicate keeping the oldest row per name, then add a UNIQUE index so
        // any future re-seed is a genuine no-op.
        version: 49,
        name: "dedupe_smart_collections_unique_name",
        up: "
DELETE FROM smart_collections
 WHERE id NOT IN (SELECT MIN(id) FROM smart_collections GROUP BY name);
CREATE UNIQUE INDEX IF NOT EXISTS idx_smart_collections_name ON smart_collections(name);
",
    },
    Migration {
        version: 50,
        name: "add_zones_dlna_native_flac",
        up: "", // Applied programmatically via add_column_if_missing
    },
    Migration {
        version: 51,
        name: "add_file_first_seen",
        up: "
CREATE TABLE IF NOT EXISTS file_first_seen (
    file_path TEXT PRIMARY KEY,
    first_seen_at REAL NOT NULL
);
",
    },
    Migration {
        // v0.9 — unified queue (kept at 52; existing release/v0.9 DBs already
        // applied it here). The actual work is done idempotently by
        // migrate_to_unified_queue() in init_schema, so the marker is a no-op.
        version: 52,
        name: "unified_queue_items",
        up: "",
    },
    Migration {
        // v0.9 — one contiguous position space per zone (streaming rows move
        // from 0..S-1 to L..L+S-1). Runs after init_schema's unified copy.
        version: 53,
        name: "unify_queue_positions",
        up: RENUMBER_QUEUE_POSITIONS_SQL,
    },
    Migration {
        version: 54,
        name: "bio_provenance",
        up: "", // Applied programmatically via add_column_if_missing
    },
    // Brought over from main in the main→release/v0.9 merge (numbered 55/56 so
    // they run on existing release/v0.9 DBs, whose highest applied version is 54).
    Migration {
        version: 55,
        name: "add_streaming_auth",
        up: "
CREATE TABLE IF NOT EXISTS streaming_auth (
    service TEXT PRIMARY KEY,
    token_data TEXT NOT NULL
);
",
    },
    // streaming_queue is also created unconditionally in init_schema (the
    // idempotent guarantee); this numbered marker mirrors main. IF NOT EXISTS.
    Migration {
        version: 56,
        name: "create_streaming_queue_table",
        up: "
CREATE TABLE IF NOT EXISTS streaming_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    source TEXT,
    source_id TEXT,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    duration_ms INTEGER DEFAULT 0
);
",
    },
    Migration {
        // Renumbered from 55 (its value on the main line): on release/v0.9,
        // 55/56 are already taken by the streaming migrations that the
        // main→v0.9 merge renumbered, so this lands at 57.
        version: 57,
        name: "add_profile_id_to_playlists",
        up: "", // Applied programmatically via add_column_if_missing (existing rows → profile 1)
    },
    // Streaming favorites: the local `favorites` table keys on an INTEGER
    // item_id, so it can only hold local-library items. Streaming items
    // (Tidal/Qobuz…) use string ids, so they get their own table here, mirroring
    // the local/streaming_queue split. Metadata (title/artist/album/cover) is
    // stored so the favorites list needs no per-item hydration for streaming.
    Migration {
        // Renumbered from 56 (its value on main): on release/v0.9, 56 is already
        // taken by create_streaming_queue_table (main→v0.9 merge), so this is 58.
        version: 58,
        name: "add_streaming_favorites",
        up: "
CREATE TABLE IF NOT EXISTS streaming_favorites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id INTEGER NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    service TEXT NOT NULL,
    service_id TEXT NOT NULL,
    title TEXT,
    artist TEXT,
    album TEXT,
    cover_url TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(profile_id, item_type, service, service_id)
);
CREATE INDEX IF NOT EXISTS idx_streaming_favorites_profile ON streaming_favorites(profile_id, item_type);
",
    },
    // Podcast subscriptions gained a `source_id` column so the client can match a
    // subscription by its streaming source id (Apple Podcasts id) rather than
    // feed_url alone — the "+ S'abonner" button stayed active because the browse
    // list keys on source_id while the subscription only stored feed_url (Fabien).
    Migration {
        version: 59,
        name: "add_source_id_to_podcast_subscriptions",
        up: "", // Applied programmatically via add_column_if_missing
    },
    // A folder on disk is what says "these files are one release". Storing it
    // makes album identity explicit instead of inferred from title + quality:
    // an edition whose discs differ in sample rate stays one album, and two
    // separate rips of the same album stay two. NULL on every pre-existing row
    // until a rescan, and the lookup falls back to title+artist then, so an
    // un-rescanned library keeps working exactly as before.
    Migration {
        version: 60,
        name: "add_folder_path_to_albums",
        up: "", // Applied programmatically via add_column_if_missing
    },
    // The quality tier the old scanner appended to album TITLES ("Album
    // (96kHz/24bit)") is machine-written noise: clients render the real quality
    // from sample_rate/bit_depth, and the folder now decides identity. Strip it
    // once, here, rather than on every scan — rewriting titles at scan time
    // would fight a user's own metadata edits.
    //
    // A rescan alone does not clean these: an album matched by its MusicBrainz
    // release id keeps the row it already had, suffix and all.
    Migration {
        version: 61,
        name: "strip_quality_suffix_from_album_titles",
        up: "", // Applied programmatically: needs the parser, not SQL.
    },
    // Stripping the suffixes leaves behind the rows the split had created: one
    // release showing up as several same-titled albums. Fold them back together
    // by the rule that now decides identity — the folder on disk — so an upgrade
    // fixes the library it finds instead of waiting for a full rescan (expensive
    // on a NAS, and it would not clean an album matched by MusicBrainz id
    // anyway). Albums in DIFFERENT folders are never folded: a CD rip and a
    // hi-res copy are meant to stay two entries.
    Migration {
        version: 62,
        name: "merge_albums_split_by_quality",
        up: "", // Applied programmatically: needs the folder rule, not SQL.
    },
    // Audio embeddings (CLAP) for acoustic "sounds-like" radio. One 512-d vector
    // per track, computed in the background analysis pass (piggybacks the
    // ReplayGain decode) and stored as a BLOB; smart_radio ranks candidates by
    // cosine similarity. Empty until the opt-in pass runs — nothing else changes.
    Migration {
        version: 63,
        name: "track_audio_embedding",
        up: "
CREATE TABLE IF NOT EXISTS track_audio_embedding (
    track_id    INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    model       TEXT    NOT NULL,
    embedding   BLOB    NOT NULL,
    analyzed_at INTEGER
);
",
    },
    // Alarm ownership (chantier 2 / C2): which profile a scheduled alarm belongs
    // to, so its playback is tagged to that person's listening history. Nullable
    // — legacy alarms have no owner and stay NULL (never guessed). Applied via
    // add_column_if_missing below for idempotency.
    Migration {
        version: 64,
        name: "add_alarms_profile_id",
        up: "", // Applied programmatically (idempotent add_column_if_missing).
    },
    // A streaming album's own track/disc numbers were lost once enqueued: the
    // unified queue stored `position` but not the album's numbering, so a
    // multi-disc streaming album showed disc 2 continuing at 25,26… instead of
    // restarting at 1. Persist them alongside the inline streaming metadata.
    // NULL on every pre-existing row and on local items (which read the numbers
    // from the joined `tracks` row), so nothing else changes.
    Migration {
        version: 65,
        name: "add_queue_item_track_disc_number",
        up: "", // Applied programmatically via add_column_if_missing (idempotent).
    },
    // Instantané d'identité des favoris : les favoris référencent des rowids
    // d'albums/pistes/artistes, mais ces ids ne survivent pas à un rescan qui
    // recrée les items (racines music déplacées, library clear, fusion de
    // doublons) — cœurs éteints et filtre « Favoris » vide (bug .18, v0.9.50).
    // On fige titre/artiste/chemin à l'ajout du favori pour re-rattacher
    // l'item vivant par identité (db::favorites_reconcile). NULL sur les
    // favoris existants ; backfillé à la première réconciliation.
    Migration {
        version: 66,
        name: "add_favorites_identity_snapshot",
        up: "", // Applied programmatically via add_column_if_missing (idempotent).
    },
    // The seeded "🖼️ Sans pochette" collection carried a placeholder rule
    // (`format is_not_empty` — i.e. every track in the library) instead of an
    // actual no-cover test; the rule engine supports `cover_path is_empty`, so
    // point the seed at it. Guarded on the exact placeholder rules string so a
    // user-customized collection is never touched; idempotent by the same
    // guard. Fresh installs seed the placeholder in migration 41 and correct it
    // here in the same run.
    Migration {
        version: 67,
        name: "fix_sans_pochette_rule",
        up: "
UPDATE smart_collections
SET rules = '[{\"field\":\"cover_path\",\"operator\":\"is_empty\",\"value\":\"\"}]'
WHERE name LIKE '%pochette%'
  AND rules = '[{\"field\":\"format\",\"operator\":\"is_not_empty\",\"value\":\"\"}]';
",
    },
    Migration {
        version: 68,
        name: "add_album_metadata_table",
        up: "
CREATE TABLE IF NOT EXISTS album_metadata (
    album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (album_id, key)
);
CREATE INDEX IF NOT EXISTS idx_album_metadata_key ON album_metadata(key);
",
    },
    // Signalements de métadonnées : jusqu'ici le seul report (image artiste)
    // squattait la table settings (clé reported_artist_image_{id}) — aucune
    // liste, aucune agrégation, aucun envoi cloud possible.
    Migration {
        version: 69,
        name: "add_metadata_reports_table",
        up: "
CREATE TABLE IF NOT EXISTS metadata_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entity TEXT NOT NULL,
    entity_id INTEGER,
    mbid TEXT,
    field TEXT,
    value TEXT,
    reason TEXT NOT NULL,
    comment TEXT,
    created_at TEXT NOT NULL,
    pushed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_metadata_reports_entity ON metadata_reports(entity, entity_id);
",
    },
    Migration {
        version: 70,
        name: "drop_dead_fip_webradios",
        // Two seeded FIP webradios have no stream left: Radio France answers 404
        // on both slugs while every other seeded FIP answers 200. They were dead
        // rows in every library created before this migration — a user picking
        // them got silence and no explanation (forum #626, Jean Valjean).
        //
        // Matched on the URL, never on the name: the name is what the user may
        // have edited, the URL is what identifies the dead stream. A station the
        // user retargeted to a working URL therefore survives, and a favourite
        // pointing at one of these rows loses a station that could not play
        // anyway. Idempotent — a second run deletes nothing.
        up: "
DELETE FROM radio_stations WHERE url = 'https://icecast.radiofrance.fr/fiplatino-hifi.aac';
DELETE FROM radio_stations WHERE url = 'https://icecast.radiofrance.fr/fiptoutnouveautoutchaud-hifi.aac';
",
    },
    Migration {
        version: 71,
        name: "add_tracks_cover_path",
        // Column added programmatically below (add_column_if_missing) so a
        // re-run on a db that already has it is a no-op rather than an error.
        up: "",
    },
    Migration {
        version: 72,
        name: "add_zones_lyrics_offset_ms",
        // Colonne ajoutee par add_column_if_missing ci-dessous : un re-passage
        // sur une base qui l'a deja est alors sans effet plutot qu'en erreur.
        up: "",
    },
    // Compilations déjà indexées, éclatées en un album par artiste (#1440) :
    // le scanner ne les produit plus, mais les bibliothèques existantes les
    // gardent. Le travail réel est dans `merge_scattered_compilations`.
    Migration {
        version: 73,
        name: "merge_scattered_compilations",
        up: "SELECT 1;",
    },
    // Corrections que la communauté propose sur les métadonnées de cet
    // instance. Elles arrivent du cloud et attendent la validation de
    // l'utilisateur : `decision` NULL = en attente.
    //
    // Local d'abord, comme les signalements : la ligne est ce qui fait foi, et
    // le renvoi de la décision au cloud est un effet de bord au-dessus. Une
    // décision prise hors ligne n'est pas perdue, elle repart au cycle suivant.
    Migration {
        version: 74,
        name: "add_metadata_proposals_table",
        up: "
CREATE TABLE IF NOT EXISTS metadata_proposals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entity TEXT NOT NULL,
    cloud_entity_id INTEGER NOT NULL,
    local_id INTEGER NOT NULL,
    title TEXT,
    artist TEXT,
    field TEXT NOT NULL,
    current_value TEXT,
    proposed_value TEXT,
    servers_count INTEGER NOT NULL DEFAULT 0,
    fetched_at TEXT NOT NULL,
    decision TEXT,
    decided_at TEXT,
    pushed_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_metadata_proposals_key
    ON metadata_proposals(entity, cloud_entity_id, field);
CREATE INDEX IF NOT EXISTS idx_metadata_proposals_pending
    ON metadata_proposals(decision, servers_count);
",
    },
    Migration {
        version: 75,
        name: "dsd_replaygain_rescale",
        up: "
-- #1638 : le decimateur DSD->PCM applique desormais l'echelle SACD (+6 dB).
-- Les ReplayGain calcules par NOTRE analyse sur l'ancienne echelle sont faux
-- de ~6 dB : on les efface pour que le sweep les recalcule. Portee stricte :
-- 1) les pistes DSD passees par l'analyse (sentinelle rg_analyzed) — les RG
--    venus des TAGS du fichier (pas de sentinelle) sont preserves ;
-- 2) les cles d'ALBUM de tout album contenant une telle piste (le gain
--    d'album mele les LUFS de toutes les pistes) — sans toucher aux gains de
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
",
    },
    Migration {
        version: 76,
        name: "cue_virtual_tracks",
        // Les colonnes elles-memes sont posees par add_column_if_missing (voir
        // plus bas) : un ALTER TABLE ici planterait en « duplicate column name »
        // sur une base neuve, ou CORE_SCHEMA les a deja.
        //
        // Ce qui se joue ici est l'UNICITE. `tracks.file_path` est UNIQUE, et
        // une feuille CUE produit N pistes pointant vers LE MEME fichier. Plutot
        // que de retirer cette contrainte — impossible en ligne sous SQLite,
        // qui exigerait de reconstruire toute la table `tracks` —, les pistes
        // virtuelles laissent `file_path` NUL (UNIQUE tolere plusieurs NULL, sur
        // les deux moteurs) et portent le vrai chemin dans `cue_media_path`.
        //
        // ⚠️ `up:` est VIDE a dessein. Le runner execute `up:` AVANT les blocs
        // `if migration.version == N`, donc un CREATE INDEX ecrit ici porterait
        // sur des colonnes pas encore ajoutees, echouerait, et un echec de
        // migration casse tout le runner (vecu chez JF, sentinelle 99).
        // Colonnes ET index sont donc poses ensemble, dans le bloc de version.
        up: "",
    },
    Migration {
        version: 77,
        name: "network_mounts_mount_state",
        // Colonnes posees par add_column_if_missing dans le bloc de version :
        // sur une base neuve elles viennent deja du CREATE TABLE de la
        // migration 7, et un ALTER TABLE ici planterait tout le runner en
        // « duplicate column name ».
        up: "",
    },
    Migration {
        version: 78,
        name: "drop_dead_radiofrance_webradios",
        // Deux stations semées n'ont plus de flux : Radio France répond 404 sur
        // `francemusiqueocoramondial` (France Musique Musiques du monde) et sur
        // `mouvxtra` (Mouv' Xtra), quand les vingt-trois autres entrées semées
        // répondent 200 avec un content-type audio (relevé le 2026-08-20).
        // Elles étaient donc, dans toute bibliothèque créée jusqu'ici, deux
        // stations qui ne pouvaient pas jouer (issue #1960).
        //
        // Ciblé sur l'URL et jamais sur le nom, comme la migration 70 : le nom
        // est ce que l'utilisateur a pu éditer, l'URL est ce qui identifie le
        // flux mort. Une station repointée par l'utilisateur vers une URL qui
        // marche survit donc. Idempotent — un second passage ne supprime rien.
        up: "
DELETE FROM radio_stations WHERE url = 'https://icecast.radiofrance.fr/francemusiqueocoramondial-hifi.aac';
DELETE FROM radio_stations WHERE url = 'https://icecast.radiofrance.fr/mouvxtra-hifi.aac';
",
    },
    Migration {
        version: 79,
        name: "albums_is_compilation",
        // Le drapeau « compilation » etait lu (TCMP), utilise au scan pour le
        // regroupement, puis jete : aucune colonne ne le stockait (#1957).
        //
        // Colonne posee par add_column_if_missing dans le bloc de version, PAS
        // par un ALTER TABLE ici : sur une base neuve CORE_SCHEMA la porte
        // deja, et l'ALTER planterait tout le runner en « duplicate column
        // name » au premier demarrage.
        up: "",
    },
    Migration {
        version: 80,
        name: "format_lowercase",
        // Replier la casse de `format`, une fois, sur les donnees deja ecrites.
        //
        // La facette des types de fichiers regroupe desormais en `LOWER(TRIM())`
        // (#1612), ce qui corrige l'AFFICHAGE pour tout le monde sans toucher a
        // la base. Mais les filtres, eux, comparent la valeur EXACTE : tant que
        // `dsd` et `DSD` coexistent en lignes, cliquer sur « DSD » ne rend que
        // la moitie des albums. L'ecran cesserait de mentir pendant que le
        // filtre continuerait — soit le pire des deux etats.
        //
        // UPDATE et non ALTER : aucune colonne n'est ajoutee ici, donc pas le
        // piege « duplicate column name » du bloc de version. Idempotent par
        // construction — `LOWER(LOWER(x))` vaut `LOWER(x)` — et la clause
        // `WHERE` evite de reecrire les lignes deja propres, ce qui compte sur
        // une bibliotheque de plusieurs dizaines de milliers d'albums.
        //
        // `tracks` en plus d'`albums` : la meme colonne y existe, alimentee par
        // le meme chemin de scan, et les memes filtres s'y appliquent.
        up: "",
    },
    Migration {
        version: 81,
        name: "format_conteneur_dsd",
        // Rendre son conteneur a chaque piste DSD deja scannee.
        //
        // `normalize_format` repliait `dsf` ET `dff` sur « dsd » : la
        // bibliotheque affichait un seul type de fichier pour deux conteneurs.
        // Il ne le fait plus, mais sans cette migration une bibliotheque
        // existante montrerait « DSD » (anciennes lignes) A COTE de « DSF »
        // (nouvelles) — le defaut d'origine sous un autre nom, et cette fois
        // par notre faute.
        //
        // L'extension du fichier est la seule source qui sache lequel des deux
        // c'etait : l'information a ete perdue a l'ecriture, elle se relit sur
        // `file_path`. Les pistes CUE laissent `file_path` NUL et portent leur
        // chemin dans `cue_media_path` (migration 76) — d'ou le COALESCE.
        //
        // L'album suit ses pistes : sa colonne `format` est un resume, et un
        // album dont toutes les pistes sont des `.dsf` est un album DSF. Un
        // album qui melangerait les deux garde « dsd », qui reste vrai et
        // reste reconnu partout (`IN ('dsd','dsf','dff')`).
        //
        // Idempotent : la clause `format = 'dsd'` ne rattrape que ce qui n'a
        // pas encore ete converti.
        up: "",
    },
    Migration {
        version: 82,
        name: "horodatage_favoris_radio_en_iso",
        // Rendre son fuseau a chaque favori radio deja enregistre.
        //
        // `radio_favorites.saved_at` avait pour defaut CURRENT_TIMESTAMP, que
        // SQLite ecrit « 2026-08-22 13:45:00 » : de l'UTC, mais SANS marqueur
        // de fuseau et avec une espace au lieu du T.
        //
        // Le client fait pourtant ce qu'il faut — `new Date(iso)` puis
        // `toLocaleDateString` — mais JavaScript, devant une chaine sans
        // fuseau, la traite comme DEJA LOCALE. L'heure UTC etait donc affichee
        // telle quelle : deux heures d'avance en ete, une en hiver. Signale par
        // Reivax66 (fil forum #1515).
        //
        // La correction n'est pas d'ecrire l'heure locale — un serveur consulte
        // depuis un autre fuseau, ou une base restauree ailleurs, mentirait
        // durablement. C'est d'horodater en UTC EXPLICITE, et de laisser le
        // client convertir, ce qu'il sait deja faire.
        //
        // Ces lignes ont toutes ete ecrites en UTC : la reparation est donc
        // purement syntaxique — espace -> T, ajout du Z. Aucune heure n'est
        // decalee ici.
        //
        // Idempotent : le filtre ne retient que ce qui n'a pas deja la forme
        // ISO (pas de T, pas de Z final).
        up: "
UPDATE radio_favorites
   SET saved_at = REPLACE(saved_at, ' ', 'T') || 'Z'
 WHERE saved_at IS NOT NULL
   AND saved_at <> ''
   AND saved_at LIKE '____-__-__ __:__:__'
   AND saved_at NOT LIKE '%Z';
",
    },
    Migration {
        version: 83,
        name: "network_mounts_unicite",
        // GgB (fil 1562, #2453) : la table ne portait aucune contrainte
        // d'unicite et create_mount inserait sans regarder — deux validations
        // du meme formulaire laissaient deux lignes que l'ecran Emplacements
        // affichait indefiniment. Purger d'abord (garder le plus petit id de
        // chaque identite), poser l'index ensuite : un index unique cree sur
        // une table non purgee echouerait sur les bases qui portent deja le
        // doublon.
        up: "
DELETE FROM network_mounts
 WHERE id NOT IN (
   SELECT MIN(id) FROM network_mounts
    GROUP BY mount_type, server, share, mount_path
 );
CREATE UNIQUE INDEX IF NOT EXISTS idx_network_mounts_identite
    ON network_mounts(mount_type, server, share, mount_path);
",
    },
    Migration {
        version: 84,
        name: "listen_history_contexte_de_lecture",
        // Rendre a chaque ecoute la trace de CE QUE l'auditeur a demande.
        //
        // `listen_history` ne conservait rien de l'intention : une piste jouee
        // seule, la meme depuis une playlist et la meme dans un album complet
        // produisaient trois lignes rigoureusement identiques. Toute rubrique
        // voulant « refleter la realite de ce qu'a voulu faire l'auditeur »
        // (FabienM, fil forum 1557) devait donc repartir de la table `albums`
        // — ce que fait `fetch_continue_listening`, d'ou « Continuer l'ecoute »
        // qui ne peut structurellement montrer qu'un album (#2441).
        //
        // `context_type` : `track`, `album`, `playlist`, `artist`, `label`.
        // `context_id` : l'identifiant de cet objet, en TEXT — une playlist
        // locale a un id numerique, un album Qobuz une chaine.
        //
        // Colonnes posees par add_column_if_missing dans le bloc de version,
        // PAS par un ALTER TABLE ici : c'est la meme regle qu'a la migration
        // 79, et l'ALTER planterait tout le runner en « duplicate column
        // name » sur une base qui les a deja.
        up: "",
    },
    Migration {
        version: 85,
        name: "favorite_facets",
        // Mettre un LABEL en favori — et, demain, un genre, un format, une
        // annee (#2442, FabienM fil 1557).
        //
        // Pourquoi une table separee plutot qu'un quatrieme `item_type` dans
        // `favorites` : `favorites.item_id` est un INTEGER NOT NULL, et un
        // label N'A PAS D'IDENTITE. Il n'existe ni table `labels`, ni route
        // bibliotheque : l'onglet Labels lit une FACETTE et selectionne par
        // CHAINE (`getLibraryFacets(['label'])`). Le faire entrer dans
        // `favorites` supposerait de promouvoir le label en entite —
        // normalisation d'un champ libre et sale, identifiants, jointures —
        // ce qui est hors gabarit ici.
        //
        // On stocke donc la valeur telle que la facette la selectionne
        // aujourd'hui. La colonne `facet` rend la table reutilisable sans
        // nouvelle migration pour genre / format / annee.
        //
        // Pas de colonne `id` : la cle naturelle (profil, facette, valeur) EST
        // la cle primaire. Cela evite aussi la divergence BIGSERIAL / TEXT que
        // la bascule SQLite -> PostgreSQL impose a toute colonne `id` (cf. la
        // migration PG 012 et l'incident #1706).
        //
        // `CREATE TABLE IF NOT EXISTS` : idempotent, et sans ALTER TABLE, donc
        // sans le piege « duplicate column name » sur une base neuve.
        up: "
CREATE TABLE IF NOT EXISTS favorite_facets (
    profile_id INTEGER NOT NULL DEFAULT 1,
    facet TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (profile_id, facet, value)
);
CREATE INDEX IF NOT EXISTS idx_favorite_facets_profile
    ON favorite_facets(profile_id, facet);
",
    },
    Migration {
        version: 86,
        name: "reparer_radios_mortes_de_l_annuaire",
        // Les stations mortes que l'utilisateur tient de NOTRE annuaire
        // (`https://mozaiklabs.fr/api/v1/radios`, bouton « + Ajouter a Tune »),
        // pas du semis. Le bouton insere l'URL sans jamais la sonder
        // (`tune-server/src/routes/radios.rs`, `add_from_web`) : une entree
        // morte cote annuaire devient une station muette cote bibliotheque.
        //
        // Sondage du 2026-08-28, redirections suivies, sur les 51 entrees que
        // l'annuaire sert ce jour-la (l'issue #1960 en decrivait 46 le 19/08) :
        // sept ne rendent pas d'audio. Deux sont deja traitees par la
        // migration 78 (les webradios Radio France). Restent les cinq d'ici.
        //
        // REMPLACEES — meme station, adresse relevee chez l'operateur :
        //
        //   * WBGO Jazz : `stream.wbgo.org` ne resout plus (curl 6, « Could not
        //     resolve host »). wbgo.org publie lui-meme ses deux flux sur sa
        //     page d'accueil et sur /listen ; le 128k MP3 rend 200 `audio/mpeg`
        //     et debite bien (638 Ko en 8 s ~ 128 kbit/s).
        //   * Reggae Classic Mix et Classic Oldies Mix : l'operateur a change
        //     d'hote et retire ses points FLAC. Son API AzuraCast
        //     (`https://online.jamminvibezradio.com/api/stations`) donne
        //     `listen_url` sur `radio.jamminvibezonline.ca` ; les deux points
        //     `stream.aac` rendent 200 `audio/aac`, les `live.flac` rendent 404.
        //
        // RETIREES — aucune adresse de remplacement verifiee :
        //
        //   * BBC Radio 3 : `stream.live.vc.bbcmedia.co.uk/bbc_radio_three`
        //     redirige vers `www.bbc.co.uk` et rend **200 `text/html`**. C'est
        //     le cas decrit par l'issue : rien n'echoue, l'auditeur n'a que du
        //     silence. Les flux HLS de la BBC rendent 410 Gone depuis ici, et
        //     un `.m3u8` ne serait de toute facon pas jouable : `resolve_direct_url`
        //     ne dereference une playlist qu'en gardant ses lignes en `http`,
        //     or une playlist HLS ne porte que des segments relatifs. Rien a
        //     mettre a la place, donc on retire.
        //   * Caribbean variety mix : la station a disparu de chez l'operateur
        //     — l'API AzuraCast ne declare plus qu'une seule station publique,
        //     et `radio.jamminvibezonline.ca/listen/caribbean/stream.aac` rend
        //     404 comme l'ancienne adresse.
        //
        // Cible sur l'URL et jamais sur le nom, comme les migrations 70 et 78 :
        // le nom est ce que l'utilisateur a pu editer, l'URL est ce qui
        // identifie le flux mort. Une station que l'utilisateur a lui-meme
        // repointee vers une adresse qui marche survit donc intacte.
        // Idempotent : un second passage ne trouve plus rien a corriger.
        //
        // Ce que cette migration NE fait PAS : elle ne corrige pas l'annuaire.
        // Les cinq entrees sont toujours servies par mozaiklabs.fr au moment
        // d'ecrire ceci ; un clic sur « + Ajouter a Tune » les remet. Le retrait
        // cote annuaire est une action de donnees sur le site, hors de ce depot
        // (voir #1960).
        up: "
UPDATE radio_stations SET url = 'https://ais-sa8.cdnstream1.com/3630_128.mp3' WHERE url = 'https://stream.wbgo.org/wbgo-hd';
UPDATE radio_stations SET url = 'https://radio.jamminvibezonline.ca/listen/reggae/stream.aac' WHERE url = 'https://online.jamminvibezradio.com/listen/reggae/live.flac';
UPDATE radio_stations SET url = 'https://radio.jamminvibezonline.ca/listen/oldies/stream.aac' WHERE url = 'https://online.jamminvibezradio.com/listen/oldies/live.flac';
DELETE FROM radio_stations WHERE url = 'http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three';
DELETE FROM radio_stations WHERE url = 'https://online.jamminvibezradio.com/listen/caribbean/live.flac';
",
    },
    Migration {
        version: 87,
        name: "temoins_poses_sur_chemin_introuvable",
        // Rendre leur chance aux pistes que le repli NFC/NFD va desormais
        // retrouver (#1865).
        //
        // Le scanner enregistre les chemins en NFC ; macOS et les partages
        // SMB/CIFS ecrivent les noms de fichiers en NFD. Les passes de fond
        // ouvraient le chemin de la base TEL QUEL, recevaient ENOENT, et
        // posaient quand meme leur temoin — « on a essaye, n'y revenons
        // pas ». La piste sortait du balayage POUR TOUJOURS. Mesure sur .18
        // le 28/08/2026 : 135 pistes dont le chemin stocke ne resout pas mais
        // dont la forme NFD existe ; 114 portaient `rg_analyzed` pour ZERO
        // `rg_track_gain`, 44 portaient `audio_embed_analyzed` pour ZERO
        // vecteur.
        //
        // Le code ne posera plus ces temoins sur un fichier introuvable (il
        // pose un report date, qui perime). Mais le code ne rattrape rien tout
        // seul : les temoins deja en base excluent ces pistes de la requete de
        // candidats, donc elles ne seront jamais relues. D'ou ce nettoyage.
        //
        // POURQUOI CE PREDICAT, ET PAS PLUS LARGE
        //
        // On ne retire le temoin que la ou il ne recouvre AUCUN resultat ET ou
        // le chemin contient au moins un octet non-ASCII — seuls les chemins
        // accentues peuvent avoir deux graphies Unicode. Sur .18 : 107 pistes
        // pour ReplayGain (au lieu de 361 sans la clause d'accent) et 50 pour
        // l'acoustique (au lieu de 53). Un fichier vraiment illisible dont le
        // nom est accentue sera reessaye UNE fois, puis re-temoigne. C'est le
        // prix, il est borne.
        //
        // `length(CAST(x AS BLOB))` rend des OCTETS la ou `length(x)` rend des
        // CARACTERES : leur inegalite est le test non-ASCII portable en
        // SQLite, sans extension regex. (La jumelle PostgreSQL 039 utilise
        // `octet_length`, qui n'existe pas partout en SQLite ancien.)
        //
        // IDEMPOTENTE par construction : ce sont des DELETE. Rejouee, elle ne
        // trouve plus rien a supprimer. `rg_skipped_oversized` est epargne :
        // ce refus-la vient d'un calcul sur la duree et le debit, pas d'un
        // acces disque — il reste valable.
        //
        // Ce qui n'est VOLONTAIREMENT pas touche : `rg_album_gain` /
        // `rg_album_peak`. Les effacer relancerait le calcul d'album
        // immediatement, donc AVANT que les pistes liberees aient leur gain —
        // meme resultat, une ecriture de plus. Le gain d'album de ces albums
        // reste donc legerement approximatif jusqu'a une reprise complete.
        up: "
DELETE FROM track_metadata
WHERE key = 'rg_analyzed'
  AND track_id IN (
    SELECT t.id FROM tracks t
    WHERE t.file_path IS NOT NULL
      AND length(CAST(t.file_path AS BLOB)) <> length(t.file_path)
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
      AND length(CAST(t.file_path AS BLOB)) <> length(t.file_path)
      AND NOT EXISTS (SELECT 1 FROM track_audio_embedding e
                       WHERE e.track_id = t.id)
  );
",
    },
    Migration {
        version: 88,
        name: "task_runs",
        // Registre des executions automatisees (#2080).
        //
        // Numerotee 88, PAS 87 : la 87 a ete prise par #1865 pendant que
        // cette PR attendait. Un numero se prend au moment de
        // l'integration, jamais a l'ouverture — la contiguite l'interdit.
        //
        // Tune lance seul une vingtaine de passes — scan de demarrage,
        // ReplayGain, enrichissement, battement de coeur, nettoyages. Aucune ne
        // laisse de trace INTERROGEABLE : le journal defile, et quand un
        // utilisateur ecrit « ca n'a rien fait », on ne peut ni le confirmer ni
        // l'infirmer. C'est cette table qui repond.
        //
        // Ce n'est PAS un journal de plus. Trois proprietes la separent d'une
        // ligne `info!` :
        //
        //   * elle survit au redemarrage (donc on peut comparer deux boots) ;
        //   * elle est bornee (rentention par passe + par age, cf.
        //     `task_run_repo`) — une table d'observabilite qui grossit sans fin
        //     finit par couter plus cher que ce qu'elle observe ;
        //   * elle ne contient ni chemin, ni cle, ni jeton : des compteurs et
        //     des verdicts. `detail` passe par un filtre avant l'ecriture.
        //
        // PAS DE COLONNE `id`. La cle naturelle (boot_id, task, seq) EST la
        // cle primaire — meme raison que `favorite_facets` (migration 85) :
        // toute colonne `id` impose la divergence AUTOINCREMENT / BIGSERIAL, et
        // la bascule SQLite -> PostgreSQL l'a deja payee cher (#1706).
        //
        // `boot_id` est l'identite d'une INCARNATION du processus, tiree une
        // fois au demarrage. C'est elle qui rend detectable la passe orpheline
        // (#2002) : au demarrage, toute ligne encore `en_cours` est fermee en
        // `interrompu`, puisque aucune passe ne survit au processus qui la
        // portait.
        //
        // `started_at` / `finished_at` sont des horodatages ABSOLUS UTC
        // ISO-8601, jamais un compteur relatif : un « il y a 3 heures » n'est
        // pas verifiable, une date l'est (lecon de `uptime_seconds`, PR #2632).
        //
        // `duration_ms` est mesure sur une horloge MONOTONE cote Rust, pas par
        // difference des deux dates : un changement d'heure systeme pendant la
        // passe rendrait la soustraction absurde (negative, meme).
        //
        // Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS, sans ALTER,
        // donc sans le piege « duplicate column name » sur une base neuve.
        up: "
CREATE TABLE IF NOT EXISTS task_runs (
    boot_id TEXT NOT NULL,
    task TEXT NOT NULL,
    seq INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    duration_ms INTEGER,
    outcome TEXT NOT NULL,
    items INTEGER,
    detail TEXT,
    PRIMARY KEY (boot_id, task, seq)
);
CREATE INDEX IF NOT EXISTS idx_task_runs_task_started ON task_runs(task, started_at);
CREATE INDEX IF NOT EXISTS idx_task_runs_outcome ON task_runs(outcome);
",
    },
    Migration {
        version: 89,
        name: "hidden_items",
        // Masquer un album sans toucher aux fichiers (#1391, Jean-Luc Cassé).
        //
        // Pourquoi une TABLE et pas une colonne `albums.is_hidden` (ce que
        // l'issue proposait, sur le modèle de `zones.is_hidden`) : une ligne
        // `zones` n'est JAMAIS supprimée (son delete est un UPDATE), une ligne
        // `albums` l'est en routine — purge post-scan, `delete_orphans`,
        // fusion de doublons, « vider la bibliothèque ». Racine music déplacée
        // → pistes purgées → album réinséré sous un nouveau rowid → drapeau
        // perdu. C'est mot pour mot le défaut déjà payé par `favorites`
        // (cœurs éteints, bug .18 v0.9.50, cf. `favorites_reconcile.rs`).
        //
        // On reprend donc la solution qui a réparé les favoris : une table de
        // marqueurs SANS clé étrangère, portant un instantané d'identité
        // (`item_name`/`item_artist`) figé au masquage, réconciliée au
        // démarrage et après chaque scan (`hidden_repo::reconcile`). Le
        // marqueur survit ainsi au rescan, au déplacement de bibliothèque et
        // à la bascule SQLite → PostgreSQL.
        //
        // PAS DE COLONNE `id` : la clé naturelle (profil, type, item) EST la
        // clé primaire — même choix que `favorite_facets` (85) et `task_runs`
        // (88), pour éviter la divergence AUTOINCREMENT / BIGSERIAL que la
        // bascule SQLite → PostgreSQL impose à toute colonne `id` (#1706).
        //
        // `item_type` est libre (`album` seul aujourd'hui) et `profile_id` est
        // ÉCRIT (toujours 1) mais jamais LU par les filtres : le masquage est
        // global maintenant, et passera par profil sans migration le jour où
        // les vues bibliothèque connaîtront le profil.
        //
        // Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.
        up: "
CREATE TABLE IF NOT EXISTS hidden_items (
    profile_id INTEGER NOT NULL DEFAULT 1,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    item_name TEXT,
    item_artist TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (profile_id, item_type, item_id)
);
CREATE INDEX IF NOT EXISTS idx_hidden_items_item ON hidden_items(item_type, item_id);
",
    },
    Migration {
        version: 90,
        name: "semis_radios_annuaire_mozaiklabs",
        // Le catalogue livré cesse d'être français-seulement (#2119).
        //
        // Le semis d'origine (migration 33) pose 24 stations, toutes
        // `country = 'France'`. Pendant ce temps NOTRE annuaire —
        // `https://mozaiklabs.fr/api/v1/radios` — en sert 51, de huit pays, et
        // le serveur le TÉLÉCHARGE DÉJÀ à chaque démarrage… pour n'en garder
        // que les logos (`refresh_radio_logos`). Une station ajoutée à
        // l'annuaire n'apparaissait donc dans le Tune de personne, et une
        // recherche « paradise » ne pouvait structurellement rien rendre.
        //
        // Cette migration pose les 25 stations de l'annuaire qui manquaient au
        // catalogue. Le texte SQL est FIGÉ dans le dépôt et non relevé au
        // démarrage : une migration ne doit jamais dépendre du réseau. Le
        // fichier porte en tête le relevé, le sondage des flux et les règles
        // d'écart — c'est là qu'il faut lire le détail, pas ici.
        //
        // Le même fichier est `include_str!` par la migration PostgreSQL 042 :
        // un seul texte pour les deux bases, donc aucune divergence possible.
        up: include_str!("../../migrations/radios/annuaire_mozaiklabs_2026_08_30.sql"),
    },
    Migration {
        version: 91,
        name: "album_distinct_pairs",
        // « Ces deux albums ne sont PAS des doublons » (#1276, Megalo,
        // forum-hifi.fr #41831 p.13 : « Tune me trouve des albums doublons
        // alors que ce sont des releases différentes »).
        //
        // Deux chemins rapprochent des albums, et l'issue vise les deux :
        // `GET /library/albums/grouped` (l'alerte) et
        // `POST /library/albums/merge-duplicates` (la fusion, qui SUPPRIME la
        // ligne perdante — le seul des deux qui ne se répare pas).
        //
        // Pourquoi une TABLE DE PAIRES sans clé étrangère, et pas une colonne
        // sur `albums` : une ligne `albums` est supprimée en routine — purge
        // post-scan, `delete_orphans`, fusion de doublons, « vider la
        // bibliothèque ». Un couple d'ids nu mourrait au premier déplacement
        // de racine, et l'arbitrage de l'utilisateur avec. C'est mot pour mot
        // le défaut déjà payé par `favorites` (#1248) puis évité par
        // `hidden_items` (#1391).
        //
        // On reprend donc la même mécanique : instantané d'identité figé DES
        // DEUX CÔTÉS (`a_name`/`a_artist`, `b_name`/`b_artist`) et
        // réconciliation aux cinq mêmes ancrages
        // (`album_distinct_repo::reconcile`), via le MÊME
        // `find_album_by_identity` que les favoris et les masquages.
        //
        // La paire est rangée en (min, max) par le repo : « A n'est pas un
        // doublon de B » ne dépend pas de l'ordre, et sans normalisation la
        // clé primaire laisserait entrer deux fois le même arbitrage.
        //
        // PAS DE COLONNE `id` : la clé naturelle (profil, album bas, album
        // haut) EST la clé primaire — même choix que `favorite_facets` (85),
        // `task_runs` (88) et `hidden_items` (89), pour éviter la divergence
        // AUTOINCREMENT / BIGSERIAL de la bascule SQLite → PostgreSQL (#1706).
        //
        // Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.
        up: "
CREATE TABLE IF NOT EXISTS album_distinct_pairs (
    profile_id INTEGER NOT NULL DEFAULT 1,
    album_a_id INTEGER NOT NULL,
    album_b_id INTEGER NOT NULL,
    a_name TEXT,
    a_artist TEXT,
    b_name TEXT,
    b_artist TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    PRIMARY KEY (profile_id, album_a_id, album_b_id)
);
CREATE INDEX IF NOT EXISTS idx_album_distinct_pairs_b ON album_distinct_pairs(album_b_id);
",
    },
    Migration {
        version: 92,
        name: "ignored_devices",
        // « Ignorer cet appareil » (#1280, Alex Campbell puis Patatorz) : faire
        // taire un APPAREIL, pas chasser ses zones une par une.
        //
        // Pourquoi une table et pas le masquage de zone déjà en place
        // (`zones.is_hidden` + `hidden_zones_by_host`, #1281) : celui-ci ne
        // porte que ce qui a DÉJÀ une zone, et seulement pour les zones
        // réseau. Il laisse l'appareil s'enregistrer comme SORTIE (la
        // découverte enregistre avant d'atteindre le garde-fou de zone), donc
        // proposé partout ; et un appareil dont la zone n'a jamais été créée
        // (`zone_auto_create` à false — le contournement donné au testeur, TV
        // filtrée, AirPlay 2 sans démon) n'a aucune ligne à masquer.
        //
        // Patron `hidden_items` (89, #1391) : table SANS clé étrangère,
        // instantané d'identité figé à l'insertion. Le marqueur ne dépend
        // d'aucune ligne `zones` — la purge des zones masquées ne l'emporte
        // pas — et survit à la bascule SQLite → PostgreSQL.
        //
        // AUCUNE troisième notion d'identité : `mac` est celle de #2803
        // (AirPlay/RAOP, déjà persistée sur `zones.mac`), `host` + `name` est
        // exactement le couple de `hidden_zones_by_host`. Le NOM est exigé
        // avec l'hôte pour ne pas bloquer un appareil différent héritant de
        // l'adresse par le DHCP (leçon du ré-ancrage #1651).
        //
        // PAS DE COLONNE `id` : `device_id` EST la clé primaire — même choix
        // que `favorite_facets` (85), `task_runs` (88) et `hidden_items` (89),
        // pour éviter la divergence AUTOINCREMENT / BIGSERIAL de la bascule
        // SQLite → PostgreSQL (#1706). Tout est TEXT : rien à rattraper côté
        // PG, contrairement à 038 et 041.
        //
        // Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.
        up: "
CREATE TABLE IF NOT EXISTS ignored_devices (
    device_id TEXT PRIMARY KEY,
    mac TEXT NOT NULL DEFAULT '',
    host TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '',
    device_type TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_ignored_devices_mac ON ignored_devices(mac);
CREATE INDEX IF NOT EXISTS idx_ignored_devices_host ON ignored_devices(host);
",
    },
    // « Dans la Smart Collection "World Music" [il] n'a pas les bons albums,
    // c'est un peu mélangé (Folk, Folk Métal, Folk Rock) » (#1426, Jean
    // Valjean, forum « F5 obligatoire »).
    //
    // Le moteur de règles fait exactement ce qu'on lui demande : le préréglage
    // livré (migrations 41 puis 47) porte `genre CONTIENT folk`, qui compile en
    // `t.genre LIKE '%folk%' COLLATE NOCASE`. « Folk Metal » et « Folk Rock »
    // entrent donc dans la collection PAR CONSTRUCTION. C'est le préréglage qui
    // promet, par son nom, plus étroit que sa règle — pas
    // `smart_collections::compile_rule`.
    //
    // Seul `folk` passe en égalité stricte. `world` et `ethnic` gardent
    // « contient », où la sous-chaîne est utile et non trompeuse (« World
    // Fusion », « Ethnic Jazz ») ; `folk` est le seul des trois dont les dérivés
    // forment des genres franchement différents. Les tags composés du type
    // « Folk, World, & Country » restent pris — par la règle `world`, le
    // `match_mode` étant `any`.
    //
    // Patron de la migration 67 (`fix_sans_pochette_rule`, jumelle PG 018) :
    // UPDATE gardé sur la chaîne de règles EXACTE du semis, donc
    //   - une collection personnalisée par l'utilisateur n'est jamais touchée,
    //   - la migration est idempotente par le même garde,
    //   - et les bibliothèques DÉJÀ installées sont réparées, ce qu'un simple
    //     changement du semis (`INSERT OR IGNORE`, création seulement) ne fait
    //     pas.
    // Sur une installation neuve, 41/47 sèment l'ancienne règle et celle-ci la
    // corrige dans la même passe.
    Migration {
        version: 93,
        name: "resserrer_folk_dans_world_music",
        up: "
UPDATE smart_collections
SET rules = '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"world\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"ethnic\"},{\"field\":\"genre\",\"operator\":\"equals\",\"value\":\"folk\"}]'
WHERE name LIKE '%World%'
  AND rules = '[{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"world\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"ethnic\"},{\"field\":\"genre\",\"operator\":\"contains\",\"value\":\"folk\"}]';
",
    },
    Migration {
        version: 94,
        name: "listen_history_rang_dans_le_contexte",
        // OU l'auditeur en etait dans l'objet qu'il avait demande.
        //
        // La migration 84 a pose CE QU'il avait demande (`context_type` /
        // `context_id`). Il manquait le RANG : sans lui, « Continuer l'ecoute »
        // peut rouvrir la bonne playlist, mais toujours a son debut. L'arbitrage
        // rendu sur #2441 est « objet courant + position », pas l'instantane de
        // la file entiere — pour un artiste ou un label, la file est batie par
        // une requete qui change d'un jour a l'autre, « conserver l'ordre » n'y
        // a aucun sens, et ecrire la file a chaque ecoute couterait un facteur
        // dix sur le volume, pour une section d'accueil.
        //
        // NULL a deux sens, et un seul est « on ne sait pas » :
        // * ligne anterieure a cette migration — inconnu, on rouvre au debut ;
        // * ecoute en lecture ALEATOIRE — le rang est deliberement laisse vide.
        //   Rejouer le meme tirage n'aurait pas de sens : on RE-TIRE. C'est la
        //   seconde moitie de l'arbitrage, ecrite ici plutot que devinee a la
        //   lecture, parce que l'etat « aleatoire » de la zone n'existe plus au
        //   moment ou l'accueil s'affiche.
        //
        // Colonne posee par add_column_if_missing dans le bloc de version, PAS
        // par un ALTER TABLE ici — meme regle qu'aux migrations 79 et 84, et
        // l'ALTER planterait le runner en « duplicate column name » sur une
        // base qui l'a deja.
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

/// La table porte-t-elle cette colonne ?
///
/// Meme lecture que `add_column_if_missing`, mais pour GARDER un UPDATE.
/// Indispensable : une base assez ancienne n'a pas encore toutes les colonnes
/// que les migrations suivantes ajouteront, et un UPDATE sur une colonne
/// absente fait echouer le batch — donc TOUT le runner, donc le demarrage
/// (vecu par le test `une_base_ancienne_gagne_le_drapeau_compilation`, qui
/// part d'une table `albums` reduite a `title` et `folder_path`).

/// SQL de la migration 80 — voir son entree dans `MIGRATIONS`.
const SQL_FORMAT_LOWERCASE: &str = "UPDATE albums SET format = LOWER(TRIM(format)) \
             WHERE format IS NOT NULL AND format != LOWER(TRIM(format)); \
             UPDATE tracks SET format = LOWER(TRIM(format)) \
             WHERE format IS NOT NULL AND format != LOWER(TRIM(format));";

/// SQL de la migration 81 — voir son entree dans `MIGRATIONS`.
const SQL_FORMAT_CONTENEUR: &str = "UPDATE tracks SET format = 'dsf' \
             WHERE format = 'dsd' \
               AND LOWER(COALESCE(file_path, cue_media_path, '')) LIKE '%.dsf'; \
             UPDATE tracks SET format = 'dff' \
             WHERE format = 'dsd' \
               AND LOWER(COALESCE(file_path, cue_media_path, '')) LIKE '%.dff'; \
             UPDATE albums SET format = 'dsf' \
             WHERE format = 'dsd' \
               AND NOT EXISTS (SELECT 1 FROM tracks t \
                               WHERE t.album_id = albums.id AND t.format <> 'dsf') \
               AND EXISTS (SELECT 1 FROM tracks t \
                           WHERE t.album_id = albums.id AND t.format = 'dsf'); \
             UPDATE albums SET format = 'dff' \
             WHERE format = 'dsd' \
               AND NOT EXISTS (SELECT 1 FROM tracks t \
                               WHERE t.album_id = albums.id AND t.format <> 'dff') \
               AND EXISTS (SELECT 1 FROM tracks t \
                           WHERE t.album_id = albums.id AND t.format = 'dff');";

fn has_column(db: &SqliteDb, table: &str, column: &str) -> bool {
    let conn = db.connection().lock().unwrap();
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names.iter().any(|name| name == column))
        })
        .unwrap_or(false)
}

/// Undo the quality tier the old scanner wrote into album titles.
///
/// Reads the candidates, strips with
/// [`crate::scanner::quality::strip_quality_suffix`] — a parser, so titles whose
/// parentheses hold something real ("Remastered", a year) are left alone — and
/// writes back only what actually changed. The `albums_fts` update trigger keeps
/// search in step.
///
/// Not merged with any same-titled album that may now exist: two rows sharing a
/// title are legitimate once the folder decides identity (a CD rip and a hi-res
/// copy), and the client tells them apart by their quality badge.
fn strip_quality_suffixes_from_album_titles(db: &SqliteDb) {
    let candidates: Vec<(i64, String)> = {
        let conn = db.connection().lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, title FROM albums \
             WHERE title LIKE '%Hz)%' OR title LIKE '%bit)%' \
                OR title LIKE '%Hz/%' OR title LIKE '%Hz %'",
        ) else {
            return;
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => return,
        }
    };

    let mut renamed = 0usize;
    for (id, title) in candidates {
        let stripped = crate::scanner::quality::strip_quality_suffix(&title);
        if stripped.is_empty() || stripped == title {
            continue;
        }
        let params: [&dyn rusqlite::types::ToSql; 2] = [&stripped, &id];
        if db
            .execute("UPDATE albums SET title = ? WHERE id = ?", &params)
            .is_ok()
        {
            renamed += 1;
        }
    }
    if renamed > 0 {
        info!(renamed, "album_quality_suffixes_stripped");
    }
}

/// Recolle les compilations déjà indexées en un album par artiste (#1440).
///
/// Le rangement Qobuz d'une compilation met chaque piste dans le dossier de SON
/// artiste : une anthologie de quarante titres devient quarante albums d'une
/// piste. Le scanner sait désormais l'éviter à l'import, mais les bibliothèques
/// existantes gardent leurs fausses entrées — 22 familles et 172 lignes sur un
/// serveur de test de 2 144 albums.
///
/// Applique EXACTEMENT les mêmes critères que le chemin de scan
/// ([`crate::scanner::compilation`]) : même titre, dossiers frères éparpillés,
/// et numéros de piste qui ne se chevauchent pas. La grappe entière est
/// abandonnée à la première collision — deux « Greatest Hits » distincts
/// commencent tous deux à la piste 1, et rien ne doit les rapprocher.
///
/// La ligne survivante est le plus petit id, comme pour la fusion par qualité :
/// c'est la plus ancienne, donc celle dont la pochette et la note sont établies.
fn merge_scattered_compilations(db: &SqliteDb) {
    use crate::scanner::compilation::is_scattered_sibling;
    use std::collections::HashMap;

    // titre normalisé -> [(id, dossier, numéros de piste)]
    let mut by_title: HashMap<String, Vec<(i64, String, Vec<i32>)>> = HashMap::new();
    {
        let conn = db.connection().lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT a.id, a.title, a.folder_path FROM albums a \
             WHERE a.folder_path IS NOT NULL AND a.folder_path <> '' ORDER BY a.id",
        ) else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) else {
            return;
        };
        let albums: Vec<(i64, String, String)> = rows.filter_map(Result::ok).collect();
        drop(stmt);
        for (id, title, folder) in albums {
            let mut nums = Vec::new();
            if let Ok(mut ts) = conn.prepare(
                "SELECT track_number FROM tracks WHERE album_id = ? AND track_number IS NOT NULL",
            ) {
                if let Ok(r) = ts.query_map([id], |row| row.get::<_, i64>(0)) {
                    nums = r.filter_map(Result::ok).map(|n| n as i32).collect();
                }
            }
            by_title
                .entry(title.trim().to_lowercase())
                .or_default()
                .push((id, folder, nums));
        }
    }

    let mut merged = 0usize;
    for (_, albums) in by_title {
        if albums.len() < 2 {
            continue;
        }
        // Grappes de dossiers frères éparpillés, par rattachement transitif.
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        for (i, (_, folder, _)) in albums.iter().enumerate() {
            match clusters.iter_mut().find(|c| {
                c.iter()
                    .any(|&j| is_scattered_sibling(folder, &albums[j].1))
            }) {
                Some(c) => c.push(i),
                None => clusters.push(vec![i]),
            }
        }

        // Une grappe peut contenir PLUSIEURS volumes portant le même titre :
        // « ALLOPOP » en compte quatre, soit 41 dossiers sous `Qobuz/`. On les
        // sépare par l'empreinte de la pochette DÉPOSÉE dans le dossier —
        // copiée à l'identique dans tous les dossiers d'un même volume, alors
        // que la jaquette extraite des pistes, elle, diffère à chaque fichier
        // (mesuré sur .18 : 41 dossiers → 4 pochettes → 4 volumes).
        let mut partitions: Vec<Vec<usize>> = Vec::new();
        for cluster in clusters.iter().filter(|c| c.len() > 1) {
            let empreintes: Vec<_> = cluster
                .iter()
                .map(|&i| crate::scanner::compilation::folder_cover_fingerprint(&albums[i].1))
                .collect();
            let (groupes, sans_pochette) = crate::scanner::compilation::group_by_cover(&empreintes);
            // Sans pochette, aucune séparation possible : la grappe reste
            // entière et c'est le chevauchement des numéros qui tranchera.
            // Mais des dossiers sans pochette au milieu d'autres qui en ont ne
            // se rattachent à rien : on ne devine pas à quel volume ils vont.
            if sans_pochette.len() == cluster.len() && cluster.len() > 1 {
                partitions.push(cluster.clone());
                continue;
            }
            for membres in groupes {
                if membres.len() > 1 {
                    partitions.push(membres.into_iter().map(|k| cluster[k]).collect());
                }
            }
        }

        for cluster in &partitions {
            // Un seul numéro en double dans la partition et on renonce : ce
            // sont des homonymes, pas les éclats d'un même disque.
            let mut seen: Vec<i32> = Vec::new();
            let mut collision = false;
            for &i in cluster {
                for n in &albums[i].2 {
                    if seen.contains(n) {
                        collision = true;
                        break;
                    }
                    seen.push(*n);
                }
                if collision {
                    break;
                }
            }
            if collision || seen.is_empty() {
                continue;
            }

            let mut ids: Vec<i64> = cluster.iter().map(|&i| albums[i].0).collect();
            ids.sort_unstable();
            let Some((keep, absorbed)) = ids.split_first() else {
                continue;
            };
            let conn = db.connection().lock().unwrap();
            for drop_id in absorbed {
                for sql in [
                    "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    "UPDATE listen_history SET album_id = ? WHERE album_id = ?",
                    "UPDATE OR IGNORE album_ratings SET album_id = ? WHERE album_id = ?",
                    "UPDATE OR IGNORE metadata_suggestions SET album_id = ? WHERE album_id = ?",
                ] {
                    let params: [&dyn rusqlite::types::ToSql; 2] = [keep, drop_id];
                    conn.execute(sql, &params[..]).ok();
                }
                let params: [&dyn rusqlite::types::ToSql; 1] = [drop_id];
                conn.execute("DELETE FROM albums WHERE id = ?", &params[..])
                    .ok();
                merged += 1;
            }
        }
    }

    if merged > 0 {
        info!(merged, "scattered_compilations_merged");
    }

    split_wrongly_merged_albums(db);
}

/// Redécoupe les albums fusionnés à tort par la régression des 0.9.66/0.9.67
/// (#1470) : un même album y porte les pistes de plusieurs disques.
///
/// Recoller ne suffit pas — les bibliothèques déjà rescannées ont le problème
/// INVERSE. Sur .18, les quatre volumes « ALLOPOP » tiennent dans un album de
/// 71 pistes, chaque numéro en quatre exemplaires.
///
/// Critère unique, le même que partout ailleurs : la pochette déposée dans le
/// dossier de chaque piste. Deux pochettes dans un album ⇒ deux disques. Le
/// plus gros groupe reste sur la ligne d'origine, qui garde ainsi sa pochette,
/// sa biographie et sa note ; les autres reçoivent une ligne neuve.
///
/// Une piste dont le dossier n'a pas de pochette ne bouge pas : on ne devine
/// pas à quel disque la rattacher.
fn split_wrongly_merged_albums(db: &SqliteDb) {
    use crate::scanner::compilation::{CoverFingerprint, folder_cover_fingerprint, group_by_cover};
    use std::collections::HashMap;

    // album -> dossier -> pistes
    let mut par_album: HashMap<i64, HashMap<String, Vec<i64>>> = HashMap::new();
    {
        let conn = db.connection().lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT t.id, t.album_id, t.file_path FROM tracks t \
             WHERE t.album_id IS NOT NULL AND t.file_path IS NOT NULL",
        ) else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) else {
            return;
        };
        for (track_id, album_id, path) in rows.filter_map(Result::ok) {
            let Some(folder) = crate::scanner::album_folder::album_folder(&path) else {
                continue;
            };
            par_album
                .entry(album_id)
                .or_default()
                .entry(folder)
                .or_default()
                .push(track_id);
        }
    }

    // Une pochette se décode UNE fois par dossier, pas une fois par piste :
    // .18 compte 49 000 pistes pour 2 300 albums, soit vingt fois trop de
    // décodages JPEG si l'on interrogeait le disque piste à piste.
    let mut empreintes: HashMap<&str, Option<CoverFingerprint>> = HashMap::new();
    for dossiers in par_album.values() {
        for dossier in dossiers.keys() {
            empreintes
                .entry(dossier.as_str())
                .or_insert_with(|| folder_cover_fingerprint(dossier));
        }
    }

    let mut separes = 0usize;
    for (album_id, par_dossier) in &par_album {
        if par_dossier.len() < 2 {
            continue;
        }
        // Ordre déterminé par le nom du dossier : l'itération d'une table de
        // hachage ne l'est pas, et c'est elle qui décide quel groupe garde la
        // ligne d'origine en cas d'égalité de taille.
        let mut dossiers: Vec<&String> = par_dossier.keys().collect();
        dossiers.sort();
        let cles: Vec<Option<CoverFingerprint>> = dossiers
            .iter()
            .map(|d| empreintes.get(d.as_str()).copied().flatten())
            .collect();
        // Les dossiers sans pochette ne bougent pas : ils restent sur la ligne
        // d'origine, faute de savoir à quel disque les rattacher.
        let (groupes, _sans_pochette) = group_by_cover(&cles);
        if groupes.len() < 2 {
            continue;
        }
        // Le plus gros groupe garde la ligne d'origine.
        let mut groupes: Vec<Vec<i64>> = groupes
            .into_iter()
            .map(|membres| {
                membres
                    .into_iter()
                    .flat_map(|k| par_dossier[dossiers[k]].iter().copied())
                    .collect()
            })
            .collect();
        groupes.sort_by_key(|pistes| std::cmp::Reverse(pistes.len()));
        let album_id = *album_id;
        let conn = db.connection().lock().unwrap();
        let Ok((titre, artist_id, year)) = conn.query_row(
            "SELECT title, artist_id, year FROM albums WHERE id = ?",
            [album_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        ) else {
            continue;
        };
        for pistes in groupes.iter().skip(1) {
            let params: [&dyn rusqlite::types::ToSql; 3] = [&titre, &artist_id, &year];
            if conn
                .execute(
                    "INSERT INTO albums (title, artist_id, year) VALUES (?, ?, ?)",
                    &params[..],
                )
                .is_err()
            {
                continue;
            }
            let nouveau = conn.last_insert_rowid();
            for piste in pistes {
                let params: [&dyn rusqlite::types::ToSql; 2] = [&nouveau, piste];
                conn.execute("UPDATE tracks SET album_id = ? WHERE id = ?", &params[..])
                    .ok();
            }
            separes += 1;
        }
    }

    if separes > 0 {
        info!(separes, "wrongly_merged_albums_split");
    }
}

/// Fold same-titled albums that share a folder back into one row.
///
/// The old quality split turned one release into several albums, one per tier.
/// Grouping by (artist, title, album folder) puts those back together while
/// leaving apart what should be: two rips in two folders keep their own rows.
///
/// The surviving row is the lowest id — the one the library has had longest, so
/// its cover, biography and rating are the ones kept. Rows referencing the
/// absorbed albums (`tracks`, `listen_history`, `album_ratings`,
/// `metadata_suggestions`) are repointed first; a rating already present on the
/// survivor wins, since `album_ratings` is unique per (album, profile).
fn merge_albums_split_by_quality(db: &SqliteDb) {
    use std::collections::HashMap;

    // (artist_id, title, album folder) -> album ids, ascending.
    let mut groups: HashMap<(i64, String, String), Vec<i64>> = HashMap::new();
    {
        let conn = db.connection().lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT a.id, COALESCE(a.artist_id, 0), a.title, \
                    (SELECT t.file_path FROM tracks t WHERE t.album_id = a.id ORDER BY t.id LIMIT 1) \
             FROM albums a ORDER BY a.id",
        ) else {
            return;
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        });
        let Ok(rows) = rows else { return };
        for (id, artist_id, title, first_path) in rows.filter_map(Result::ok) {
            // No track, no folder, nothing to merge on.
            let Some(folder) = first_path
                .as_deref()
                .and_then(crate::scanner::album_folder::album_folder)
                .filter(|f| !f.is_empty())
            else {
                continue;
            };
            groups
                .entry((artist_id, title.trim().to_lowercase(), folder))
                .or_default()
                .push(id);
        }
    }

    let mut merged = 0usize;
    for ((_, _, folder), ids) in groups {
        let Some((keep, absorbed)) = ids.split_first() else {
            continue;
        };
        if absorbed.is_empty() {
            // Single row: still record its folder so the scanner recognises it
            // without a rescan.
            let params: [&dyn rusqlite::types::ToSql; 2] = [&folder, keep];
            db.execute(
                "UPDATE albums SET folder_path = ? WHERE id = ? AND folder_path IS NULL",
                &params,
            )
            .ok();
            continue;
        }
        for drop_id in absorbed {
            for sql in [
                "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                "UPDATE listen_history SET album_id = ? WHERE album_id = ?",
                "UPDATE OR IGNORE album_ratings SET album_id = ? WHERE album_id = ?",
                "UPDATE OR IGNORE metadata_suggestions SET album_id = ? WHERE album_id = ?",
            ] {
                let params: [&dyn rusqlite::types::ToSql; 2] = [keep, drop_id];
                // A table may not exist on an old database — ignore and go on.
                db.execute(sql, &params).ok();
            }
            let params: [&dyn rusqlite::types::ToSql; 1] = [drop_id];
            db.execute("DELETE FROM albums WHERE id = ?", &params).ok();
            merged += 1;
        }
        let params: [&dyn rusqlite::types::ToSql; 2] = [&folder, keep];
        db.execute("UPDATE albums SET folder_path = ? WHERE id = ?", &params)
            .ok();
        let params: [&dyn rusqlite::types::ToSql; 1] = [keep];
        db.execute(
            "UPDATE albums SET track_count = \
             (SELECT COUNT(*) FROM tracks WHERE album_id = albums.id) WHERE id = ?",
            &params,
        )
        .ok();
    }
    if merged > 0 {
        info!(merged, "albums_split_by_quality_merged");
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

    // Ce qui reste à faire, AVANT de le faire : sans ce décompte il n'y avait
    // rien à annoncer — ni dans le journal, ni à l'écran d'attente (#1701). Le
    // « +1 » est la passe finale (colonnes de sûreté, file unifiée, ANALYZE),
    // qui tourne à chaque démarrage et pèse, elle aussi, sur une grosse base.
    let floor = current_version.max(if tables_exist { 1 } else { 0 });
    let pending = MIGRATIONS.iter().filter(|m| m.version > floor).count();
    let started = std::time::Instant::now();
    migration_status::begin("sqlite", pending + 1);
    if pending > 0 {
        info!(
            from = floor,
            to = latest_version(),
            pending,
            "migration_start"
        );
    }

    let mut done = 0usize;
    for migration in MIGRATIONS {
        if migration.version <= floor {
            continue;
        }

        info!(
            version = migration.version,
            name = migration.name,
            step = done + 1,
            total = pending + 1,
            "migration_applying"
        );
        migration_status::advance(migration.name, done);
        let step_started = std::time::Instant::now();

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
        if migration.version == 72 {
            // Décalage des paroles par zone — forum #1328.
            add_column_if_missing(
                db,
                "zones",
                "lyrics_offset_ms",
                "INTEGER NOT NULL DEFAULT 0",
            );
        }
        if migration.version == 71 {
            // Per-track cover — see migration 71 and forum #1312.
            add_column_if_missing(db, "tracks", "cover_path", "TEXT");
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
        if migration.version == 50 {
            add_column_if_missing(db, "zones", "dlna_native_flac", "INTEGER DEFAULT 0");
        }
        if migration.version == 54 {
            // Bio provenance/attribution (CC BY-SA) + freshness, on both tables.
            for table in ["artists", "albums"] {
                add_column_if_missing(db, table, "bio_source", "TEXT");
                add_column_if_missing(db, table, "bio_source_url", "TEXT");
                add_column_if_missing(db, table, "bio_license", "TEXT");
                add_column_if_missing(db, table, "bio_lang", "TEXT");
                add_column_if_missing(db, table, "bio_fetched_at", "TEXT");
            }
        }
        if migration.version == 57 {
            // Scope playlists per profile. Existing rows default to profile 1 (Default).
            // (Version 57 on the v0.9 line; 55 on main — see the migration entry.)
            add_column_if_missing(db, "playlists", "profile_id", "INTEGER NOT NULL DEFAULT 1");
        }
        if migration.version == 59 {
            // Match subscriptions by streaming source id (Apple Podcasts id), not
            // just feed_url — keeps the browse "S'abonner" button in sync (Fabien).
            add_column_if_missing(db, "podcast_subscriptions", "source_id", "TEXT");
        }
        if migration.version == 60 {
            // The album's folder on disk — see the migration's comment.
            add_column_if_missing(db, "albums", "folder_path", "TEXT");
            db.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_albums_folder_path \
                 ON albums(folder_path) WHERE folder_path IS NOT NULL",
            )
            .ok();
        }
        if migration.version == 61 {
            strip_quality_suffixes_from_album_titles(db);
        }
        if migration.version == 62 {
            merge_albums_split_by_quality(db);
        }
        if migration.version == 73 {
            merge_scattered_compilations(db);
        }
        if migration.version == 64 {
            add_column_if_missing(db, "alarms", "profile_id", "INTEGER");
        }
        if migration.version == 65 {
            // Per-album numbering for streaming queue items — see the migration's
            // comment. add_column_if_missing keeps this a no-op on a fresh DB
            // (CORE_SCHEMA already carries the columns) and safe on partial re-runs.
            add_column_if_missing(db, "queue_items", "track_number", "INTEGER");
            add_column_if_missing(db, "queue_items", "disc_number", "INTEGER");
        }
        if migration.version == 76 {
            // Pistes virtuelles d'une feuille CUE : `cue_media_path` porte le
            // vrai fichier audio, `file_path` restant NUL pour ces pistes.
            add_column_if_missing(db, "tracks", "cue_media_path", "TEXT");
            add_column_if_missing(db, "tracks", "cue_start_ms", "INTEGER");
            add_column_if_missing(db, "tracks", "cue_end_ms", "INTEGER");
            // L'index vient APRES les colonnes, et ici plutot que dans `up:` —
            // que le runner execute avant ce bloc. Il rend une identite aux
            // pistes virtuelles : sans lui, chaque scan les recreerait, faute de
            // pouvoir les retrouver (`file_path` etant NUL pour toutes).
            let _ = db.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_tracks_cue_identity \
                 ON tracks(cue_media_path, cue_start_ms) \
                 WHERE cue_media_path IS NOT NULL;",
            );
        }
        if migration.version == 77 {
            // Le dialecte qui a REELLEMENT monte le partage, et l'issue du
            // dernier essai. Sans `smb_version`, le remontage au demarrage
            // repartait de zero avec `vers=3.0` en dur : le partage SMB 1.0 de
            // Philippe Landes montait depuis l'assistant puis se perdait au
            // premier redemarrage (#1834). Sans `mount_state`, cet echec ne se
            // voyait nulle part (#1916).
            add_column_if_missing(db, "network_mounts", "smb_version", "TEXT");
            add_column_if_missing(db, "network_mounts", "mount_state", "TEXT");
            add_column_if_missing(db, "network_mounts", "last_mount_error", "TEXT");
        }
        // 80 et 81 : `up:` VIDE a dessein, le SQL vit ici sous un garde.
        // Un UPDATE sur une colonne que la base n'a pas encore fait echouer le
        // batch, et un echec de migration casse TOUT le runner.
        if (migration.version == 80 || migration.version == 81)
            && has_column(db, "albums", "format")
            && has_column(db, "tracks", "format")
        {
            let sql = if migration.version == 80 {
                SQL_FORMAT_LOWERCASE
            } else {
                SQL_FORMAT_CONTENEUR
            };
            if let Err(e) = db.execute_batch(sql) {
                warn!(version = migration.version, error = %e, "migration_format_ignoree");
            }
        }

        if migration.version == 79 {
            // Drapeau « compilation » de l'album (#1957). DEFAULT 0 : les
            // lignes existantes valent « non », et le prochain scan leve le
            // drapeau sur les disques qu'il regroupe en Various Artists.
            add_column_if_missing(db, "albums", "is_compilation", "INTEGER DEFAULT 0");
        }

        db.execute(
            "INSERT INTO _migrations (version, name) VALUES (?, ?)",
            &[
                &migration.version as &dyn rusqlite::types::ToSql,
                &migration.name,
            ],
        )?;

        done += 1;
        info!(
            version = migration.version,
            name = migration.name,
            ms = step_started.elapsed().as_millis() as u64,
            "migration_applied"
        );
    }

    migration_status::advance("contrôles finaux", done);

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
    add_column_if_missing(db, "zones", "dlna_native_flac", "INTEGER DEFAULT 0");
    // Opt-in: serve ALAC directly to a renderer that decodes it (bit-perfect,
    // no FLAC transcode). Off by default — ALAC and AAC share the audio/mp4
    // MIME, so it can't be auto-detected safely.
    add_column_if_missing(db, "zones", "alac_passthrough", "INTEGER DEFAULT 0");
    // Opt-in : servir l'AAC tel quel au renderer qui le décode, au lieu de le
    // transcoder en FLAC. Demandé par Marco Polo (#1424) pour un Marantz SR7009
    // et un Denon RC12, qui l'acceptent nativement.
    //
    // L'AAC étant déjà compressé avec perte, le gain n'est pas la qualité — le
    // transcodage n'ajoute pas de perte audible — mais la RÉACTIVITÉ : plus de
    // réencodage avant le premier octet, et pas de charge processeur.
    //
    // Éteint par défaut, comme alac_passthrough et pour la même raison : un
    // renderer qui ANNONCE l'AAC peut le refuser dans un conteneur ou à un
    // débit donné. Détecté automatiquement, cela produirait un silence
    // inexpliqué — le pire symptôme. Celui qui active sait ce que son appareil
    // fait vraiment ; les autres ne voient aucun changement.
    add_column_if_missing(db, "zones", "aac_passthrough", "INTEGER DEFAULT 0");
    // Opt-in: transcode lossless to WAV/LPCM (not FLAC) for this DLNA renderer.
    // Skips the slow native FLAC encoder for hi-res AND avoids renderers whose
    // ALAC decoder pops at start (Yves, LHC-56). Overrides alac_passthrough.
    add_column_if_missing(db, "zones", "dlna_lpcm", "INTEGER DEFAULT 0");
    // Opt-in: cap this DLNA renderer's output to 16-bit. Some renderers advertise
    // `audio/flac` (so Tune sends hi-res FLAC/ALAC direct) but only decode 16-bit
    // internally — 24-bit direct plays SILENCE (Ruark R3, Yves forum #1137). This
    // forces a 16-bit downconvert (kept as FLAC) instead of direct passthrough,
    // without regressing renderers that genuinely play 24-bit.
    add_column_if_missing(db, "zones", "dlna_cap_16bit", "INTEGER DEFAULT 0");
    // Opt-in: serve genuine 24-bit WAV to this DLNA renderer (instead of the
    // 16-bit LPCM fallback). Only safe on renderers that advertise `audio/L24`
    // in their GetProtocolInfo Sink — the UI only offers the toggle when the
    // capability probe reports `lpcm24`. The 24-bit WAV is advertised WITHOUT
    // the 16-bit-only `DLNA.ORG_PN=LPCM` profile so a strict renderer no longer
    // maps it back to 16-bit and reads misaligned samples (the #1137 silence
    // class). Off by default; overrides dlna_lpcm/dlna_cap_16bit when set.
    add_column_if_missing(db, "zones", "dlna_wav24", "INTEGER DEFAULT 0");
    // Per-zone SetAVTransportURI→Play delay in ms (default 0 = use the config /
    // device-name default). Lets a renderer with a cold-start under-run (first
    // seconds hachées — Cyrille, Yamaha R-N2000A) buffer before its transport
    // clock starts, the network analogue of the local ring-buffer prefill.
    // Overrides `[device_delays]` / `dlna_play_delay_ms` from config.
    add_column_if_missing(db, "zones", "dlna_play_delay_ms", "INTEGER DEFAULT 0");
    // Physical host (IP) of the renderer, used to dedup DLNA zones across
    // rediscovery: a renderer that comes back with a NEW UPnP UUID (Denon Ceol
    // N12 after a restart) must reconnect to its existing zone instead of
    // spawning a duplicate (forum #942).
    add_column_if_missing(db, "zones", "host", "TEXT");
    // MAC of the renderer (Phase B of the MAC-identity chantier): the durable
    // cross-protocol key. A Bluesound Node discovered as BluOS + DLNA +
    // OpenHome must end up with ONE zone even when names and UUIDs all
    // differ and the IP changes (forum #1239, Bilou: 3 « Node » zones).
    add_column_if_missing(db, "zones", "mac", "TEXT");

    add_column_if_missing(db, "listen_history", "source_id", "TEXT");
    add_column_if_missing(db, "listen_history", "album_id", "INTEGER");
    add_column_if_missing(db, "listen_history", "profile_id", "INTEGER");
    // Ce que l'auditeur a demande au moment du clic sur « Lire » (migration
    // 84, #2441). TEXT pour les deux : l'identifiant peut etre un entier
    // local ou une chaine de service de streaming.
    add_column_if_missing(db, "listen_history", "context_type", "TEXT");
    add_column_if_missing(db, "listen_history", "context_id", "TEXT");
    // Rang de la piste dans cet objet (migration 94, #2441). NULL = inconnu
    // (ligne d'avant la migration) OU tirage aleatoire, cas ou l'on RE-TIRE
    // au lieu de rejouer le meme ordre.
    add_column_if_missing(db, "listen_history", "context_position", "INTEGER");

    // Playlists scoped per profile (migration v55). Safety pass so DBs from any
    // prior version get the column regardless of which migration they came from.
    add_column_if_missing(db, "playlists", "profile_id", "INTEGER NOT NULL DEFAULT 1");

    add_column_if_missing(db, "alarms", "days_of_week", "TEXT DEFAULT '1111111'");
    add_column_if_missing(db, "alarms", "multi_zone_ids", "TEXT");

    // Constat du dernier montage SMB (migration v77). Passe de surete : le
    // remontage au demarrage SELECTionne `smb_version`, donc une base qui
    // arriverait ici sans la colonne ferait echouer la requete — et plus AUCUN
    // partage ne serait remonte, pour tout le monde.
    add_column_if_missing(db, "network_mounts", "smb_version", "TEXT");
    add_column_if_missing(db, "network_mounts", "mount_state", "TEXT");
    add_column_if_missing(db, "network_mounts", "last_mount_error", "TEXT");

    // Drapeau « compilation » de l'album (migration v79). Passe de surete du
    // meme ordre que `smb_version` ci-dessus, et pour la meme raison : le
    // SELECT commun des albums (`album_repo::sql::select_album`) lit
    // `a.is_compilation`. Une base qui arriverait ici sans la colonne ferait
    // echouer TOUTES les requetes d'albums — bibliotheque vide, partout.
    add_column_if_missing(db, "albums", "is_compilation", "INTEGER DEFAULT 0");

    // Podcast subscriptions matched by streaming source id (migration v59). Safety
    // pass so DBs from any prior version get the column (Fabien: "S'abonner" stays).
    add_column_if_missing(db, "podcast_subscriptions", "source_id", "TEXT");

    // Instantané d'identité des favoris (migration v66) : titre/artiste/chemin
    // figés à l'ajout, pour re-rattacher un favori quand un rescan renouvelle
    // les rowids (racines music déplacées, library clear — bug .18). Passe de
    // sûreté idempotente ; voir db::favorites_reconcile.
    add_column_if_missing(db, "favorites", "item_name", "TEXT");
    add_column_if_missing(db, "favorites", "item_artist", "TEXT");
    add_column_if_missing(db, "favorites", "item_path", "TEXT");

    // Provenance d'un embedding CLAP (#1732 phase 1) : NULL = analysé sur le
    // fichier, 'inherited:<id>' = copié depuis une jumelle (le DSD est exclu
    // de l'analyse ; l'héritage est sa seule voie vers les ambiances). Passe
    // de sûreté idempotente, PG : migration 025.
    add_column_if_missing(db, "track_audio_embedding", "source", "TEXT");

    // Persistent "date added" side table (survives full rescan). CREATE IF NOT
    // EXISTS here too so DBs from any prior version get it regardless of which
    // migration version they came from.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_first_seen (file_path TEXT PRIMARY KEY, first_seen_at REAL NOT NULL);",
    )
    .ok();

    // Ensure streaming_queue exists BEFORE the unified-queue copy reads it. It
    // used to be created lazily on first write, so on a fresh DB the unified
    // migration and the Deezer/streaming connect path could hit "no such table:
    // streaming_queue" (forum #951: Bilou/Yan, still seen in v0.8.287+ on Windows
    // where a numbered migration can fail silently). IF NOT EXISTS = no-op
    // otherwise. Runs every startup, so it is the idempotent guarantee across the
    // main↔release/v0.9 merge (the numbered migrations may collide/skip).
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS streaming_queue (\
            id INTEGER PRIMARY KEY AUTOINCREMENT,\
            zone_id INTEGER NOT NULL,\
            position INTEGER NOT NULL,\
            source TEXT,\
            source_id TEXT,\
            title TEXT,\
            artist TEXT,\
            album TEXT,\
            cover_url TEXT,\
            duration_ms INTEGER DEFAULT 0\
        );",
    )
    .ok();

    // Streaming favorites (migration v58 on the v0.9 line); re-create
    // unconditionally so DBs from any prior version get it regardless of which
    // migration they came from.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS streaming_favorites (\
            id INTEGER PRIMARY KEY AUTOINCREMENT,\
            profile_id INTEGER NOT NULL DEFAULT 1,\
            item_type TEXT NOT NULL,\
            service TEXT NOT NULL,\
            service_id TEXT NOT NULL,\
            title TEXT,\
            artist TEXT,\
            album TEXT,\
            cover_url TEXT,\
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),\
            UNIQUE(profile_id, item_type, service, service_id)\
        );",
    )
    .ok();

    // v0.9 — unify play_queue + streaming_queue into queue_items. Idempotent and
    // reads streaming_queue (just ensured above), so it is safe on fresh DBs and
    // on DBs that skipped the numbered unified-queue migration.
    migrate_to_unified_queue(db);

    db.execute_batch("ANALYZE;").ok();
    info!("sqlite_analyze_complete");

    migration_status::finish();
    info!(
        applied = pending,
        ms = started.elapsed().as_millis() as u64,
        "migrations_complete"
    );

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
///
/// `pub(crate)` pour le test `pg_schema_parity`, qui rejoue cette liste sur une
/// base nue et la compare au schema neuf de `pg_migrate.rs` (#2111).
#[cfg(feature = "postgres")]
pub(crate) const PG_MIGRATIONS: &[(i32, &str, &str)] = &[
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
    (
        8,
        "schema_sync",
        include_str!("../../migrations/postgres/008_schema_sync.sql"),
    ),
    (
        9,
        "smart_playlists_match_mode",
        include_str!("../../migrations/postgres/009_smart_playlists_match_mode.sql"),
    ),
    (
        10,
        "numeric_column_types",
        include_str!("../../migrations/postgres/010_numeric_column_types.sql"),
    ),
    (
        11,
        "history_numeric_column_types",
        include_str!("../../migrations/postgres/011_history_numeric_column_types.sql"),
    ),
    (
        12,
        "integer_id_columns",
        include_str!("../../migrations/postgres/012_integer_id_columns.sql"),
    ),
    (
        13,
        "numeric_column_types_remaining",
        include_str!("../../migrations/postgres/013_numeric_column_types_remaining.sql"),
    ),
    (
        14,
        "album_folder_path",
        include_str!("../../migrations/postgres/014_album_folder_path.sql"),
    ),
    (
        15,
        "track_audio_embedding",
        include_str!("../../migrations/postgres/015_track_audio_embedding.sql"),
    ),
    (
        16,
        "alarms_profile_id",
        include_str!("../../migrations/postgres/016_alarms_profile_id.sql"),
    ),
    (
        17,
        "favorites_identity",
        include_str!("../../migrations/postgres/017_favorites_identity.sql"),
    ),
    (
        18,
        "fix_sans_pochette_rule",
        include_str!("../../migrations/postgres/018_fix_sans_pochette_rule.sql"),
    ),
    (
        19,
        "album_metadata",
        include_str!("../../migrations/postgres/019_album_metadata.sql"),
    ),
    (
        20,
        "metadata_reports",
        include_str!("../../migrations/postgres/020_metadata_reports.sql"),
    ),
    (
        21,
        "track_cover_path",
        include_str!("../../migrations/postgres/021_track_cover_path.sql"),
    ),
    (
        22,
        "zone_lyrics_offset",
        include_str!("../../migrations/postgres/022_zone_lyrics_offset.sql"),
    ),
    (
        23,
        "metadata_proposals",
        include_str!("../../migrations/postgres/023_metadata_proposals.sql"),
    ),
    (
        24,
        "dsd_replaygain_rescale",
        include_str!("../../migrations/postgres/024_dsd_replaygain_rescale.sql"),
    ),
    (
        25,
        "embedding_source",
        include_str!("../../migrations/postgres/025_embedding_source.sql"),
    ),
    (
        26,
        "queue_items_numbering",
        include_str!("../../migrations/postgres/026_queue_items_numbering.sql"),
    ),
    (
        27,
        "network_mounts_mount_state",
        include_str!("../../migrations/postgres/027_network_mounts_mount_state.sql"),
    ),
    (
        28,
        "albums_is_compilation",
        include_str!("../../migrations/postgres/028_albums_is_compilation.sql"),
    ),
    // Jumelle de la migration SQLite 80. Les deux listes sont SEPAREES —
    // `run_migrations` ne prend qu'un `SqliteDb` — donc une correction de
    // donnees doit etre ecrite DEUX fois, sans quoi un seul moteur est repare
    // (#1612).
    (
        29,
        "format_lowercase",
        include_str!("../../migrations/postgres/029_format_lowercase.sql"),
    ),
    (
        30,
        "format_conteneur_dsd",
        include_str!("../../migrations/postgres/030_format_conteneur_dsd.sql"),
    ),
    // Jumelle PG de la migration SQLite 76 (#1763), posee avec dix migrations
    // de retard : le chantier CUE n'avait touche que le schema NEUF cote
    // PostgreSQL (#2111).
    (
        31,
        "cue_colonnes_et_identite",
        include_str!("../../migrations/postgres/031_cue_colonnes_et_identite.sql"),
    ),
    // Douze reglages de zone qui n'existaient pas cote PostgreSQL — dont
    // `dlna_wav24`, absente meme du schema neuf. L'ecriture etait avalee en
    // silence et l'API repondait « enregistre » (#2111).
    (
        32,
        "zones_reglages_manquants",
        include_str!("../../migrations/postgres/032_zones_reglages_manquants.sql"),
    ),
    // `podcast_subscriptions.source_id` n'arrivait par AUCUNE des trois voies
    // d'une base PG neuve : ni script numerote, ni ENSURE_COLUMNS, ni
    // ENSURE_TABLES. Elle n'existait que dans PG_FULL_SCHEMA, qui ne tourne
    // QUE pendant la migration SQLite -> PG. Or `routes/podcasts.rs` la
    // SELECT : l'ecran Podcasts tombait sur toute installation PostgreSQL
    // partie de zero. Meme famille que l'incident `queue_items` du .15.
    (
        33,
        "podcast_source_id",
        include_str!("../../migrations/postgres/033_podcast_source_id.sql"),
    ),
    // 034 était sur disque sans être enregistrée ici : les installations
    // PostgreSQL n'ont donc JAMAIS reçu la conversion de
    // `radio_favorites.saved_at` en TEXTE, alors que SQLite l'a eue
    // (migration 82). Le test de contiguïté de PG_MIGRATIONS l'a révélé en
    // refusant le trou entre 33 et 35 — on la répare donc ici, à la même
    // occasion : elle est idempotente et sans perte (le `DO $$` ne convertit
    // que si la colonne est encore numérique).
    (
        34,
        "radio_favorites_saved_at_texte",
        include_str!("../../migrations/postgres/034_radio_favorites_saved_at_texte.sql"),
    ),
    (
        35,
        "network_mounts_unicite",
        include_str!("../../migrations/postgres/035_network_mounts_unicite.sql"),
    ),
    // #2468 : 005 cree cette colonne en INTEGER sur une base neuve et 013 ne
    // convertit que TEXT/VARCHAR. Une migration nouvelle est indispensable :
    // les installations ayant deja enregistre la version 13 ne la rejouent pas.
    (
        36,
        "bookmarks_position_bigint",
        include_str!("../../migrations/postgres/036_bookmarks_position_bigint.sql"),
    ),
    // Jumelle de la migration SQLite 84. `listen_history` n'a jamais su d'ou
    // venait une ecoute : sans ces deux colonnes, la meme piste jouee seule,
    // depuis une playlist ou dans un album donnent trois lignes identiques,
    // et aucune rubrique ne peut refleter l'intention de l'auditeur (#2441).
    (
        37,
        "listen_history_contexte_de_lecture",
        include_str!("../../migrations/postgres/037_listen_history_contexte_de_lecture.sql"),
    ),
    // Favori d'une VALEUR de facette — le label d'abord (#2442). Table
    // separee : `favorites.item_id` est un entier, un label n'a pas
    // d'identite. Pendant de la migration SQLite 85.
    // Numerotee 38 : 36 et 37 sont parties dans les lots fusionnes entre-temps,
    // et la contiguite interdit de reserver un numero d'avance.
    (
        38,
        "favorite_facets",
        include_str!("../../migrations/postgres/038_favorite_facets.sql"),
    ),
    // Jumelle de la migration SQLite 87 (#1865) : retirer les temoins de
    // passe poses sur des fichiers introuvables faute de repli NFC/NFD, sans
    // quoi le correctif ne rattrape AUCUNE des bibliotheques deja balayees.
    // Numerotee 39 : le suivant libre a l'ouverture de la PR, pas un numero
    // reserve d'avance — la contiguite l'interdit.
    (
        39,
        "temoins_poses_sur_chemin_introuvable",
        include_str!("../../migrations/postgres/039_temoins_poses_sur_chemin_introuvable.sql"),
    ),
    // Registre des executions automatisees (#2080). Jumelle de la migration
    // SQLite 88. Sans cette entree, la route d'observabilite rendrait une
    // erreur SQL sur tout le parc PostgreSQL — .15, .18, Docker.
    //
    // Numerotee 40, PAS 39 : la 39 a ete prise par #1865 pendant que cette
    // PR attendait.
    (
        40,
        "task_runs",
        include_str!("../../migrations/postgres/040_task_runs.sql"),
    ),
    // Albums masqués (#1391). Jumelle de la migration SQLite 89 — les deux
    // listes sont SÉPARÉES : écrite d'un seul côté, la table manquerait à
    // tout le parc PostgreSQL et chaque vue bibliothèque y rendrait une
    // erreur SQL (le filtre « pas masqué » la nomme).
    (
        41,
        "hidden_items",
        include_str!("../../migrations/postgres/041_hidden_items.sql"),
    ),
    // Jumelle de la migration SQLite 90 : le catalogue de radios cesse d'être
    // français-seulement (#2119). LE MÊME FICHIER que la migration SQLite —
    // pas une copie : le SQL du semis est volontairement portable
    // (`INSERT ... SELECT ... WHERE NOT EXISTS`), et deux fichiers jumeaux
    // finiraient par diverger. Sans cette entrée, tout le parc PostgreSQL —
    // .15, .18, Docker — resterait aux 24 stations françaises.
    (
        42,
        "semis_radios_annuaire_mozaiklabs",
        include_str!("../../migrations/radios/annuaire_mozaiklabs_2026_08_30.sql"),
    ),
    // « Ces deux albums ne sont pas des doublons » (#1276). Jumelle de la
    // migration SQLite 91 — mêmes deux listes séparées : sans cette entrée,
    // la table manquerait à tout le parc PostgreSQL et les deux routes de
    // rapprochement d'albums y rendraient une erreur SQL.
    (
        43,
        "album_distinct_pairs",
        include_str!("../../migrations/postgres/043_album_distinct_pairs.sql"),
    ),
    // Appareils ignorés (#1280). Jumelle de la migration SQLite 92 — les deux
    // listes sont SÉPARÉES : écrite d'un seul côté, la table manquerait à tout
    // le parc PostgreSQL (.15, .18, Docker) et « ignorer cet appareil » y
    // rendrait une erreur SQL.
    (
        44,
        "ignored_devices",
        include_str!("../../migrations/postgres/044_ignored_devices.sql"),
    ),
    // Jumelle de la SQLite 93 (#1426). C'est une migration de DONNÉES, pas de
    // schéma : sans elle, le parc PostgreSQL garderait « genre contient folk »
    // dans le préréglage « World Music » — et donc Folk Metal et Folk Rock —
    // pour toujours. PostgreSQL ne sème pas les collections intelligentes
    // lui-même : elles y sont arrivées par la bascule SQLite → PostgreSQL.
    (
        45,
        "resserrer_folk_dans_world_music",
        include_str!("../../migrations/postgres/045_resserrer_folk_dans_world_music.sql"),
    ),
    // #2441 — le RANG dans l'objet demandé. La 037 avait posé CE QUE
    // l'auditeur avait demandé ; sans le rang, « Continuer l'écoute » rouvre
    // la bonne playlist mais toujours à sa première piste. NULL vaut aussi
    // « on re-tire » : en lecture aléatoire, le rang n'est délibérément pas
    // écrit.
    (
        46,
        "listen_history_rang_dans_le_contexte",
        include_str!("../../migrations/postgres/046_listen_history_rang_dans_le_contexte.sql"),
    ),
    // #2860 — `listen_history.album_id` est TEXT et `albums.id` BIGINT : la
    // jointure de « Continuer l'ecoute » rend `operator does not exist:
    // text = bigint`, avalee par `unwrap_or_default()`, donc section vide sur
    // TOUTE installation PostgreSQL. La 012 convertit deja cette colonne, mais
    // elle ne l'a jamais vue : `album_id` n'arrive par aucun script numerote,
    // seulement par ENSURE_COLUMNS, joue APRES. Cette migration rejoue la
    // conversion maintenant que la colonne existe.
    (
        47,
        "listen_history_album_id_bigint",
        include_str!("../../migrations/postgres/047_listen_history_album_id_bigint.sql"),
    ),
    // #2886 — `zones.volume` passe d'INTEGER a DOUBLE PRECISION. Pas de
    // jumelle SQLite : SQLite est a typage dynamique et l'affinite INTEGER ne
    // convertit une decimale que si c'est SANS PERTE, donc une base deja
    // creee stocke deja 0,4 tel quel. PostgreSQL, lui, REFUSE un f64 dans une
    // colonne integer — sans cette migration, tout reglage de volume echouerait
    // sur PG. Le test `un_volume_fractionnaire_survit_a_une_colonne_declaree_integer`
    // tient l'autre moitie de la preuve.
    (
        48,
        "zones_volume_a_virgule",
        include_str!("../../migrations/postgres/048_zones_volume_a_virgule.sql"),
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

    // Heal databases whose schema_version was created by the SQLite→PG data
    // migration with `version TEXT` (pg_migrate.rs before this fix) while this
    // runner and the migration scripts use INTEGER. On such a database
    // `COALESCE(MAX(version), 0)` mixes text and integer and PG aborts — the
    // server then panics at startup (JF, v0.9.13: "COALESCE types text and
    // integer cannot be matched"). Values are always digit strings, so the
    // in-place cast is safe; no-op once the column is integer.
    let version_type: Option<String> = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_name = 'schema_version' AND column_name = 'version'",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("pg inspect schema_version: {e}"))?;
    if matches!(
        version_type.as_deref(),
        Some("text") | Some("character varying")
    ) {
        sqlx::raw_sql(
            "ALTER TABLE schema_version \
             ALTER COLUMN version TYPE INTEGER USING version::integer",
        )
        .execute(pool)
        .await
        .map_err(|e| format!("pg heal schema_version type: {e}"))?;
        info!("pg_schema_version_column_healed_text_to_integer");
    }

    // What has already been applied?
    let mut current: i32 =
        sqlx::query_scalar::<_, i32>("SELECT COALESCE(MAX(version), 0) FROM schema_version")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("pg read schema_version: {e}"))?;

    // Databases created by the SQLite→PG data migration are stamped with the
    // sentinel version 99 ("schema as of the migration date"). 99 outranks
    // every numbered migration, so scripts added AFTER that date (009
    // smart_playlists match_mode, 010 numeric column types…) were skipped
    // forever: JF's force-scan added nothing (album resolution fails with
    // `operator does not exist: text = bigint`, the exact drift 010 repairs)
    // and album views 500'd. All numbered scripts are idempotent (007's
    // destructive DROP is guarded on the broken-id condition since this fix),
    // so drop the sentinel and let the normal loop bring the database to the
    // real latest, recording true version rows along the way.
    if current == 99 {
        sqlx::raw_sql("DELETE FROM schema_version WHERE version = 99")
            .execute(pool)
            .await
            .map_err(|e| format!("pg drop sentinel 99: {e}"))?;
        current =
            sqlx::query_scalar::<_, i32>("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_one(pool)
                .await
                .map_err(|e| format!("pg reread schema_version: {e}"))?;
        info!(
            resume_from = current,
            "pg_sentinel_99_dropped_replaying_idempotent_migrations"
        );
    }

    // Même décompte que côté SQLite : de quoi annoncer l'avancement au lieu de
    // laisser croire à un serveur planté (#1701). Le « +1 » est l'ANALYZE final.
    let pending = PG_MIGRATIONS
        .iter()
        .filter(|&&(v, _, _)| v > current)
        .count();
    let started = std::time::Instant::now();
    migration_status::begin("postgres", pending + 1);
    if pending > 0 {
        info!(
            from = current,
            to = pg_latest_version(),
            pending,
            "pg_migration_start"
        );
    }

    let mut done = 0usize;
    for &(version, name, sql) in PG_MIGRATIONS {
        if version <= current {
            continue;
        }

        info!(
            version,
            name,
            step = done + 1,
            total = pending + 1,
            "pg_migration_applying"
        );
        migration_status::advance(name, done);
        let step_started = std::time::Instant::now();

        // Each migration file manages its own BEGIN/COMMIT, so we
        // execute the raw SQL directly.
        sqlx::raw_sql(sql)
            .execute(pool)
            .await
            .map_err(|e| format!("pg migration {version} ({name}): {e}"))?;

        done += 1;
        info!(
            version,
            name,
            ms = step_started.elapsed().as_millis() as u64,
            "pg_migration_applied"
        );
    }

    migration_status::advance("contrôles finaux", done);

    // Run ANALYZE on key tables for the query planner.
    sqlx::raw_sql("ANALYZE artists; ANALYZE albums; ANALYZE tracks;")
        .execute(pool)
        .await
        .ok();
    info!("pg_analyze_complete");

    migration_status::finish();
    info!(
        applied = pending,
        ms = started.elapsed().as_millis() as u64,
        "pg_migrations_complete"
    );

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
    use std::fs;
    use std::path::Path;

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
    fn unified_queue_exists_after_migrations() {
        // Regression class (tester Yacine, Synology DS418j — originally on the
        // legacy streaming_queue): startup's orphan-queue cleanup must never
        // hit a missing table on a fresh DB. Since the v0.9 unified queue the
        // cleanup targets queue_items — assert run_migrations creates it and
        // that the exact startup DELETE succeeds.
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let exists: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='queue_items'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "queue_items must exist after run_migrations");

        // The startup orphan cleanup DELETE must not error on a migrated DB.
        conn.execute_batch("DELETE FROM queue_items WHERE zone_id NOT IN (SELECT id FROM zones)")
            .expect("orphan cleanup DELETE must succeed on a migrated DB");
    }

    #[test]
    fn migrations_are_idempotent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        run_migrations(&db).unwrap();
        assert_eq!(current_version(&db).unwrap(), latest_version());
    }

    /// #1426 — le préréglage « World Music » livré ne doit plus porter
    /// « genre CONTIENT folk », qui ramasse « Folk Metal » et « Folk Rock ».
    /// `world` et `ethnic` gardent « contient », où la sous-chaîne est utile.
    #[test]
    fn world_music_livre_avec_folk_en_egalite_stricte() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let regles: String = conn
            .query_row(
                "SELECT rules FROM smart_collections WHERE name LIKE '%World%'",
                [],
                |r| r.get(0),
            )
            .expect("le préréglage « World Music » doit être semé");

        assert!(
            regles.contains(r#"{"field":"genre","operator":"equals","value":"folk"}"#),
            "folk doit être en égalité stricte : {regles}"
        );
        assert!(
            !regles.contains(r#"{"field":"genre","operator":"contains","value":"folk"}"#),
            "« contient folk » ramasse Folk Metal et Folk Rock (#1426) : {regles}"
        );
        for garde in [
            "\"contains\",\"value\":\"world\"",
            "\"contains\",\"value\":\"ethnic\"",
        ] {
            assert!(
                regles.contains(garde),
                "seul `folk` change ; `world` et `ethnic` restent en « contient » : {regles}"
            );
        }
    }

    /// La correction #1426 est gardée sur la chaîne de règles EXACTE du semis :
    /// une collection que l'utilisateur a retouchée ne doit JAMAIS être
    /// réécrite sous ses pieds — c'est la contrepartie de réparer les
    /// bibliothèques déjà installées.
    #[test]
    fn world_music_personnalisee_nest_pas_reecrite() {
        const SUR_MESURE: &str = r#"[{"field":"genre","operator":"contains","value":"gnawa"}]"#;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        {
            // On remet la base dans l'état d'un utilisateur qui avait déjà
            // retouché sa collection AVANT que la 93 n'arrive : règles sur
            // mesure, et la 93 retirée du registre pour qu'elle rejoue.
            let conn = db.connection().lock().unwrap();
            conn.execute(
                "UPDATE smart_collections SET rules = ?1 WHERE name LIKE '%World%'",
                rusqlite::params![SUR_MESURE],
            )
            .unwrap();
            // `>= 93` et non `= 93` : le runner repart de MAX(version), donc
            // effacer la seule 93 ne la ferait PAS rejouer dès qu'une
            // migration ultérieure existe — le test se serait tu en croyant
            // prouver quelque chose. Il ne dépend plus de sa place dans la
            // liste (#2441, migration 94).
            conn.execute("DELETE FROM _migrations WHERE version >= 93", [])
                .unwrap();
        }
        run_migrations(&db).unwrap();
        assert_eq!(
            current_version(&db).unwrap(),
            latest_version(),
            "la 93 doit bien avoir rejoué"
        );

        let conn = db.connection().lock().unwrap();
        let regles: String = conn
            .query_row(
                "SELECT rules FROM smart_collections WHERE name LIKE '%World%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            regles, SUR_MESURE,
            "une collection personnalisée doit rester intacte (#1426)"
        );
        drop(conn);

        // Contre-épreuve DANS le test : sans elle, « rien n'a bougé » serait
        // aussi vrai d'une migration vide. On repose la chaîne LIVRÉE, on fait
        // rejouer la 93, et cette fois elle DOIT mordre.
        const LIVREE: &str = concat!(
            r#"[{"field":"genre","operator":"contains","value":"world"},"#,
            r#"{"field":"genre","operator":"contains","value":"ethnic"},"#,
            r#"{"field":"genre","operator":"contains","value":"folk"}]"#
        );
        {
            let conn = db.connection().lock().unwrap();
            conn.execute(
                "UPDATE smart_collections SET rules = ?1 WHERE name LIKE '%World%'",
                rusqlite::params![LIVREE],
            )
            .unwrap();
            // Même raison qu'au-dessus : c'est le plancher MAX(version) qui
            // décide de ce qui rejoue.
            conn.execute("DELETE FROM _migrations WHERE version >= 93", [])
                .unwrap();
        }
        run_migrations(&db).unwrap();
        let conn = db.connection().lock().unwrap();
        let regles: String = conn
            .query_row(
                "SELECT rules FROM smart_collections WHERE name LIKE '%World%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            regles, LIVREE,
            "sur la chaîne livrée, la 93 doit mordre — sinon le garde ci-dessus \
             ne prouve rien"
        );
        assert!(regles.contains(r#""operator":"equals","value":"folk""#));
    }

    /// Forum #1328 : décalage des paroles par zone. Vérifie la colonne ET son
    /// défaut — un défaut non nul décalerait les paroles de tout le monde.
    #[test]
    fn zones_have_a_lyrics_offset_defaulting_to_zero() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let conn = db.connection().lock().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(zones)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            cols.iter().any(|c| c == "lyrics_offset_ms"),
            "zones.lyrics_offset_ms manquante : {cols:?}"
        );
        conn.execute_batch("INSERT INTO zones (name, output_type) VALUES ('z', 'local');")
            .unwrap();
        let off: i64 = conn
            .query_row(
                "SELECT lyrics_offset_ms FROM zones WHERE name = 'z'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(off, 0, "une zone neuve ne doit decaler aucune parole");
    }

    /// Forum #1312: a track needs a cover of its own, so a folder the scanner
    /// had to name itself cannot lend one file's artwork to all the others.
    #[test]
    fn tracks_have_their_own_cover_column() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let conn = db.connection().lock().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(tracks)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            cols.iter().any(|c| c == "cover_path"),
            "tracks.cover_path missing: {cols:?}"
        );
    }

    /// Les pistes virtuelles CUE reposent sur trois colonnes ET un index unique
    /// partiel. Les colonnes seules ne suffisent pas : sans l'index, rien
    /// n'identifie une piste virtuelle et chaque scan les recréerait.
    #[test]
    fn cue_virtual_tracks_have_their_columns_and_identity_index() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let conn = db.connection().lock().unwrap();

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(tracks)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for c in ["cue_media_path", "cue_start_ms", "cue_end_ms"] {
            assert!(
                cols.iter().any(|x| x == c),
                "tracks.{c} manquante: {cols:?}"
            );
        }

        let idx: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='tracks'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            idx.iter().any(|i| i == "idx_tracks_cue_identity"),
            "index d'identité CUE manquant: {idx:?}"
        );
    }

    /// Le point qui rend tout le dispositif possible : `file_path` est UNIQUE,
    /// et plusieurs pistes d'une même feuille CUE partagent un seul fichier.
    /// C'est légal UNIQUEMENT parce que `UNIQUE` tolère plusieurs `NULL`.
    /// Si ce comportement changeait, le découpage CUE casserait en silence.
    #[test]
    fn unique_file_path_still_tolerates_several_nulls() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let conn = db.connection().lock().unwrap();
        for (start, title) in [(0, "Aria"), (32_000, "Variatio 1"), (125_493, "Variatio 2")] {
            conn.execute(
                "INSERT INTO tracks (title, file_path, cue_media_path, cue_start_ms) \
                 VALUES (?1, NULL, '/m/gould.ape', ?2)",
                rusqlite::params![title, start],
            )
            .expect("plusieurs pistes CUE doivent coexister sur un même fichier");
        }
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracks WHERE cue_media_path = '/m/gould.ape'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);

        // Mais deux pistes au MÊME début sur le MÊME fichier restent interdites :
        // c'est ce qui empêche un rescan de dupliquer la bibliothèque.
        let dup = conn.execute(
            "INSERT INTO tracks (title, file_path, cue_media_path, cue_start_ms) \
             VALUES ('doublon', NULL, '/m/gould.ape', 0)",
            [],
        );
        assert!(dup.is_err(), "l'index d'identité n'empêche pas le doublon");
    }

    /// The upgrade path relies on `add_column_if_missing`, so pin its contract:
    /// it adds the column once and a re-run is a no-op rather than an error.
    /// Exercised on a throwaway table — mangling `tracks` here would only fight
    /// its FTS triggers and test nothing extra.
    #[test]
    fn add_column_if_missing_is_idempotent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db.execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY);")
            .unwrap();
        add_column_if_missing(&db, "probe", "cover_path", "TEXT");
        add_column_if_missing(&db, "probe", "cover_path", "TEXT");
        let conn = db.connection().lock().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(probe)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            cols.iter().filter(|c| *c == "cover_path").count(),
            1,
            "cover_path should be added exactly once: {cols:?}"
        );
    }

    /// GgB (fil 1562, #2453) : deux validations du meme formulaire creaient
    /// deux lignes identiques que l'ecran Emplacements affichait sans fin — la
    /// table ne portait aucune contrainte d'unicite. Ce test part d'une base
    /// qui porte DEJA le doublon, rejoue les migrations, et exige la purge
    /// puis le refus de tout nouveau doublon. Il ECHOUE contre le code d'avant.
    #[test]
    fn une_base_avec_doublon_de_montage_est_purgee_et_verrouillee() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE network_mounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mount_type TEXT NOT NULL DEFAULT 'smb',
                server TEXT NOT NULL,
                share TEXT NOT NULL,
                mount_path TEXT NOT NULL,
                username TEXT,
                password TEXT,
                active INTEGER DEFAULT 1
            );
            INSERT INTO network_mounts (server, share, mount_path) \
             VALUES ('192.168.1.159', 'ROSEDISK', '/mnt/rose');
            INSERT INTO network_mounts (server, share, mount_path) \
             VALUES ('192.168.1.159', 'ROSEDISK', '/mnt/rose');
            INSERT INTO network_mounts (server, share, mount_path) \
             VALUES ('192.168.1.159', 'AUTRE', '/mnt/autre');",
        )
        .unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM network_mounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n, 2,
            "le doublon doit etre purge, les identites distinctes gardees"
        );
        let doublon = conn.execute(
            "INSERT INTO network_mounts (mount_type, server, share, mount_path) \
             VALUES ('smb', '192.168.1.159', 'ROSEDISK', '/mnt/rose')",
            [],
        );
        assert!(
            doublon.is_err(),
            "un nouveau doublon doit etre refuse par l'index unique"
        );
    }

    /// Le remontage au démarrage `SELECT`e `smb_version` sur `network_mounts`.
    /// Si la colonne manque, la requête échoue et **plus aucun partage n'est
    /// remonté, pour tout le monde** — une base d'avant la v77 casserait donc
    /// le remontage qu'elle est censée réparer.
    ///
    /// Ce test part d'une base portant la table dans sa forme d'origine (celle
    /// de la migration 7), rejoue les migrations, et exige les trois colonnes.
    /// Il ÉCHOUE contre le code d'avant (#1834, #1916).
    #[test]
    fn une_base_ancienne_gagne_les_colonnes_de_constat_du_montage() {
        let db = SqliteDb::open_in_memory().unwrap();
        // La forme d'avant : ni dialecte retenu, ni constat.
        db.execute_batch(
            "CREATE TABLE network_mounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mount_type TEXT NOT NULL DEFAULT 'smb',
                server TEXT NOT NULL,
                share TEXT NOT NULL,
                mount_path TEXT NOT NULL,
                username TEXT,
                password TEXT,
                active INTEGER DEFAULT 1
            );
            INSERT INTO network_mounts (server, share, mount_path) \
             VALUES ('192.168.1.159', 'ROSEDISK', '/mnt/rose');",
        )
        .unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(network_mounts)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for attendue in ["smb_version", "mount_state", "last_mount_error"] {
            assert!(
                cols.iter().any(|c| c == attendue),
                "`{attendue}` manque après migration : {cols:?}"
            );
        }

        // Et la ligne existante survit : une migration qui reconstruirait la
        // table ferait perdre son partage à l'utilisateur, ce qui est
        // exactement le symptôme qu'on corrige.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM network_mounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "le partage enregistré a disparu à la migration");

        // Colonnes vides tant qu'aucun montage n'a été tenté : NUL se lit
        // « jamais tenté », et se distingue de 'failed'.
        let etat: Option<String> = conn
            .query_row("SELECT mount_state FROM network_mounts", [], |r| r.get(0))
            .unwrap();
        assert!(
            etat.is_none(),
            "mount_state devrait être NUL, vaut {etat:?}"
        );
    }

    /// Le SELECT commun des albums (`album_repo::sql::select_album`) lit
    /// `a.is_compilation` (#1957). Si la colonne manque, ce n'est pas la
    /// pastille qui manque : c'est TOUTE requête d'album qui échoue —
    /// bibliothèque vide, partout.
    ///
    /// Ce test part d'une base portant `albums` dans une forme d'AVANT la v79,
    /// rejoue les migrations, et exige la colonne, la ligne existante, et un
    /// `SELECT` réel avec la clause du dépôt. Il ÉCHOUE contre le code d'avant.
    /// Les favoris radio deja enregistres retrouvent leur fuseau (#1515).
    ///
    /// Le scenario reel : une base d'UTILISATEUR, ou la table existe deja avec
    /// des lignes ecrites par l'ancien defaut CURRENT_TIMESTAMP, qu'on met a
    /// jour. Ma premiere version rejouait `run_migrations` deux fois sur une
    /// base neuve — le second passage ne faisait RIEN, les versions appliquees
    /// etant enregistrees, et le test echouait pour la mauvaise raison.
    ///
    /// Il ECHOUE contre le code d'avant : c'est sa raison d'etre.
    #[test]
    fn les_favoris_radio_retrouvent_leur_fuseau() {
        let db = SqliteDb::open_in_memory().unwrap();
        // La forme d'avant : la table existe, ses horodatages n'ont pas de
        // fuseau. `init_schema` la laissera en place (CREATE TABLE IF NOT
        // EXISTS), comme sur la machine d'un utilisateur.
        db.execute_batch(
            "CREATE TABLE radio_favorites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                artist TEXT DEFAULT '',
                station_name TEXT DEFAULT '',
                cover_url TEXT,
                stream_url TEXT,
                saved_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(title, artist)
            );
            INSERT INTO radio_favorites (title, artist, saved_at) VALUES
               ('Come on In', 'Bridge City Sinners', '2026-08-22 13:45:00'),
               ('Pistol',     'Kings Of Leon',       '2026-08-22T11:00:00Z'),
               ('Sans heure', 'Personne',            NULL),
               ('Vide',       'Personne2',           '');",
        )
        .unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let lire = |titre: &str| -> Option<String> {
            conn.query_row(
                "SELECT saved_at FROM radio_favorites WHERE title = ?1",
                [titre],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };

        // Espace -> T, et le Z ajoute. Aucune heure n'est DECALEE : ces lignes
        // etaient deja en UTC, il leur manquait seulement de le dire.
        assert_eq!(lire("Come on In").as_deref(), Some("2026-08-22T13:45:00Z"));
        // Une ligne deja au bon format n'est pas retouchee — sans quoi un
        // second passage lui collerait un second Z.
        assert_eq!(lire("Pistol").as_deref(), Some("2026-08-22T11:00:00Z"));
        // Ni le NUL ni le vide ne deviennent une date inventee.
        assert_eq!(lire("Sans heure"), None);
        assert_eq!(lire("Vide").as_deref(), Some(""));
    }

    #[test]
    fn une_base_ancienne_gagne_le_drapeau_compilation() {
        let db = SqliteDb::open_in_memory().unwrap();
        // La forme d'avant : aucune colonne pour le drapeau.
        db.execute_batch(
            "CREATE TABLE albums (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                artist_id INTEGER,
                year INTEGER,
                folder_path TEXT
            );
            INSERT INTO albums (title) VALUES ('Jazz sur Seine');",
        )
        .unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(albums)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            cols.iter().any(|c| c == "is_compilation"),
            "`is_compilation` manque après migration : {cols:?}"
        );

        // La ligne existante survit : une migration qui reconstruirait la table
        // ferait perdre sa bibliothèque à l'utilisateur.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "l'album enregistré a disparu à la migration");

        // DEFAULT 0 : un album jamais rescanné se lit « pas une compilation »,
        // pas NUL — la vue album n'a pas à distinguer trois états.
        let drapeau: Option<i64> = conn
            .query_row("SELECT is_compilation FROM albums", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            drapeau,
            Some(0),
            "is_compilation devrait valoir 0, vaut {drapeau:?}"
        );

        // Et la requête que le serveur exécute vraiment passe.
        conn.query_row(
            "SELECT a.is_compilation FROM albums a WHERE a.id = 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("le SELECT du dépôt d'albums doit passer après migration");
    }

    /// Forum #626: two seeded FIP webradios whose stream Radio France no longer
    /// serves. A fresh library must not carry them, and a library that already
    /// seeded them must lose them.
    #[test]
    fn dead_fip_webradios_are_gone() {
        const DEAD: [&str; 2] = [
            "https://icecast.radiofrance.fr/fiplatino-hifi.aac",
            "https://icecast.radiofrance.fr/fiptoutnouveautoutchaud-hifi.aac",
        ];
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let count_dead = || {
            let conn = db.connection().lock().unwrap();
            DEAD.iter()
                .map(|url| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM radio_stations WHERE url = ?1",
                        [url],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap()
                })
                .sum::<i64>()
        };

        // Fresh install: never seeded.
        assert_eq!(count_dead(), 0, "dead FIP webradios seeded on a fresh db");

        // Existing install: re-insert them the way an older seed did, then let
        // the migration run again. It must clean them out, and leave the living
        // FIP stations alone.
        {
            let conn = db.connection().lock().unwrap();
            for url in DEAD {
                conn.execute(
                    "INSERT INTO radio_stations (name, url, genre, country) VALUES ('x', ?1, 'g', 'France')",
                    [url],
                )
                .unwrap();
            }
        }
        assert_eq!(count_dead(), 2, "test fixture did not insert");

        let live_before = {
            let conn = db.connection().lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM radio_stations WHERE url LIKE '%fip%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };

        // Replaying the migration is what an upgrade does for a db still below
        // version 70; force it here since this db is already at latest.
        {
            let conn = db.connection().lock().unwrap();

            let m = MIGRATIONS.iter().find(|m| m.version == 70).unwrap();
            conn.execute_batch(m.up).unwrap();
            // Idempotent: a second pass must also be a no-op, not an error.
            conn.execute_batch(m.up).unwrap();
        }

        assert_eq!(count_dead(), 0, "migration 70 left a dead station behind");
        let live_after = {
            let conn = db.connection().lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM radio_stations WHERE url LIKE '%fip%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(
            live_after,
            live_before - 2,
            "migration 70 removed more than the two dead stations"
        );
    }

    /// Issue #1960 : deux webradios Radio France semées dont le flux répond
    /// 404. Une bibliothèque neuve ne doit pas les porter, et une bibliothèque
    /// qui les a déjà semées doit les perdre.
    #[test]
    fn dead_radiofrance_webradios_are_gone() {
        const DEAD: [&str; 2] = [
            "https://icecast.radiofrance.fr/francemusiqueocoramondial-hifi.aac",
            "https://icecast.radiofrance.fr/mouvxtra-hifi.aac",
        ];
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let count_dead = || {
            let conn = db.connection().lock().unwrap();
            DEAD.iter()
                .map(|url| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM radio_stations WHERE url = ?1",
                        [url],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap()
                })
                .sum::<i64>()
        };

        // Installation neuve : jamais semées.
        assert_eq!(count_dead(), 0, "station morte semée sur une base neuve");

        // Installation existante : on les réinsère comme l'ancien seed le
        // faisait, puis on rejoue la migration. Elle doit les retirer, et
        // laisser les stations vivantes tranquilles.
        {
            let conn = db.connection().lock().unwrap();
            for url in DEAD {
                conn.execute(
                    "INSERT INTO radio_stations (name, url, genre, country) VALUES ('x', ?1, 'g', 'France')",
                    [url],
                )
                .unwrap();
            }
        }
        assert_eq!(count_dead(), 2, "la fixture n'a rien inséré");

        let live_before = {
            let conn = db.connection().lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM radio_stations WHERE url LIKE '%radiofrance%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };

        // Rejouer la migration est ce que fait une mise à jour d'une base
        // restée sous la version 78 ; on la force ici, cette base étant déjà
        // à jour.
        {
            let conn = db.connection().lock().unwrap();
            let m = MIGRATIONS.iter().find(|m| m.version == 78).unwrap();
            conn.execute_batch(m.up).unwrap();
            // Idempotent : un second passage doit aussi être un non-événement.
            conn.execute_batch(m.up).unwrap();
        }

        assert_eq!(
            count_dead(),
            0,
            "la migration 78 a laissé une station morte"
        );
        let live_after = {
            let conn = db.connection().lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM radio_stations WHERE url LIKE '%radiofrance%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(
            live_after,
            live_before - 2,
            "la migration 78 a retiré plus que les deux stations mortes"
        );
    }

    /// Le catalogue que Tune LIVRE, relu tel qu'une installation neuve le
    /// reçoit — pas le texte du semis, la table après migrations.
    ///
    /// Contrôle de FORME, jamais de réseau (issue #1960). Sonder les flux
    /// depuis la CI serait instable et bombarderait des tiers ; ce que ce test
    /// garde, c'est ce qu'aucune sonde ne rattrape : un champ vide, un doublon,
    /// un schéma que le lecteur ne sait pas ouvrir.
    #[test]
    fn le_catalogue_livre_a_une_forme_valide() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let stations: Vec<(String, String, String, String)> = {
            let conn = db.connection().lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT name, url, genre, country FROM radio_stations ORDER BY id")
                .unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    ))
                })
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };

        // Plancher, pas égalité : ajouter une station vérifiée ne doit pas
        // casser le test, la vider en douce doit le casser.
        assert!(
            stations.len() >= 20,
            "le catalogue livré est tombé à {} stations",
            stations.len()
        );

        for (name, url, genre, country) in &stations {
            assert!(!name.trim().is_empty(), "station sans nom pour l'URL {url}");
            assert!(!url.trim().is_empty(), "station « {name} » sans URL");
            assert!(!genre.trim().is_empty(), "station « {name} » sans genre");
            assert!(!country.trim().is_empty(), "station « {name} » sans pays");

            // Les DEUX seuls schémas que le lecteur sait ouvrir : toute lecture
            // de station finit sur un GET reqwest (`resolve_direct_url` puis
            // `decode_radio_stream_to_pcm`), et reqwest ne parle que http et
            // https. Même règle que la validation à la saisie de la PR #2635.
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "station « {name} » : schéma que le lecteur ne sait pas ouvrir — {url}"
            );
            assert_eq!(
                url.trim(),
                url,
                "station « {name} » : URL entourée d'espaces — {url:?}"
            );
            assert!(
                !url.contains(' '),
                "station « {name} » : espace dans l'URL — {url:?}"
            );
        }

        let urls: std::collections::HashSet<&str> =
            stations.iter().map(|(_, u, _, _)| u.as_str()).collect();
        assert_eq!(
            urls.len(),
            stations.len(),
            "deux stations livrées partagent la même URL"
        );
        let names: std::collections::HashSet<&str> =
            stations.iter().map(|(n, _, _, _)| n.as_str()).collect();
        assert_eq!(
            names.len(),
            stations.len(),
            "deux stations livrées portent le même nom"
        );
    }

    /// Le semis ne doit rien planter qu'une migration ultérieure arrache.
    ///
    /// C'est l'erreur que les migrations 70 et 78 ont dû réparer : le semis
    /// posait des stations mortes, une migration les retirait derrière. Le
    /// texte du semis et les `DELETE` doivent rester d'accord — sinon une
    /// installation neuve reçoit une station puis la perd au premier
    /// redémarrage (issue #1960).
    #[test]
    fn le_semis_ne_pose_rien_qu_une_migration_supprime() {
        let seed = MIGRATIONS
            .iter()
            .find(|m| m.name == "seed_default_radios")
            .expect("la migration de semis des radios a disparu");

        // Relever la 2e chaîne SQL de chaque INSERT, en tenant compte du
        // quote doublé (`'Mouv'''`) : un split naïf sur `'` décale tout et
        // rendrait des URL vides — une preuve fabriquée.
        fn litteraux_sql(ligne: &str) -> Vec<String> {
            let mut out = Vec::new();
            let mut chars = ligne.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '\'' {
                    continue;
                }
                let mut buf = String::new();
                loop {
                    match chars.next() {
                        Some('\'') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            buf.push('\'');
                        }
                        Some('\'') | None => break,
                        Some(c) => buf.push(c),
                    }
                }
                out.push(buf);
            }
            out
        }

        let mut semees: Vec<String> = seed
            .up
            .lines()
            .filter(|l| {
                l.trim_start()
                    .starts_with("INSERT OR IGNORE INTO radio_stations")
            })
            .filter_map(|l| litteraux_sql(l).into_iter().nth(1))
            .collect();
        assert!(
            semees.len() >= 20,
            "seulement {} URL relevées dans le semis — le relevé s'est cassé",
            semees.len()
        );

        // Le SECOND semis, celui de l'annuaire (migration 90, #2119). Il pose
        // 25 stations de plus, dont trois que la migration 86 a dû repointer :
        // s'il reprenait une adresse morte, une installation neuve la
        // recevrait puis la perdrait — exactement le défaut que ce test garde.
        // Sa forme est portable (`INSERT ... SELECT ... WHERE NOT EXISTS`), et
        // l'URL réellement posée est celle de la garde.
        let annuaire = MIGRATIONS
            .iter()
            .find(|m| m.name == "semis_radios_annuaire_mozaiklabs")
            .expect("le semis de l'annuaire a disparu");
        let depuis_annuaire: Vec<String> = annuaire
            .up
            .lines()
            .filter(|l| {
                l.trim_start()
                    .starts_with("WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = ")
            })
            .filter_map(|l| litteraux_sql(l).into_iter().next())
            .collect();
        assert!(
            depuis_annuaire.len() >= 25,
            "seulement {} URL relevées dans le semis de l'annuaire — le relevé s'est cassé",
            depuis_annuaire.len()
        );
        semees.extend(depuis_annuaire);
        for url in &semees {
            assert!(
                url.starts_with("http"),
                "le relevé du semis a rendu {url:?} au lieu d'une URL"
            );
        }

        for m in MIGRATIONS.iter() {
            for line in m.up.lines() {
                let line = line.trim();
                if !line.starts_with("DELETE FROM radio_stations WHERE url = ") {
                    continue;
                }
                let Some(supprimee) = line.split('\'').nth(1) else {
                    continue;
                };
                assert!(
                    !semees.iter().any(|s| s == supprimee),
                    "la migration {} ({}) supprime {supprimee}, que le semis pose encore",
                    m.version,
                    m.name
                );
            }
        }

        // Même règle pour les REPOINTAGES. La migration 86 remplace l'adresse
        // de trois stations dont le flux est mort ; elle ne s'applique qu'aux
        // bases déjà semées, pas aux lignes posées APRÈS elle. Un semis qui
        // reprendrait l'ancienne adresse livrerait une station muette à toute
        // installation neuve, sans que rien ne la rattrape jamais.
        for m in MIGRATIONS.iter() {
            for line in m.up.lines() {
                let line = line.trim();
                if !line.starts_with("UPDATE radio_stations SET url = ") {
                    continue;
                }
                let litteraux = litteraux_sql(line);
                let (Some(_neuve), Some(ancienne)) = (litteraux.first(), litteraux.get(1)) else {
                    continue;
                };
                assert!(
                    !semees.iter().any(|s| s == ancienne),
                    "la migration {} ({}) repointe {ancienne}, que le semis pose encore",
                    m.version,
                    m.name
                );
            }
        }
    }

    /// #2119 — le catalogue LIVRÉ n'est plus français-seulement.
    ///
    /// Contre-épreuve permanente : elle se lit sur la table après migrations,
    /// pas sur le texte du semis. Vider le fichier de l'annuaire, oublier
    /// d'enregistrer la migration 90, ou la faire arracher par une migration
    /// ultérieure font tomber ce test — trois façons de reproduire le défaut
    /// sans s'en apercevoir.
    #[test]
    fn le_catalogue_livre_couvre_plusieurs_pays() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let stations: Vec<(String, String)> = {
            let conn = db.connection().lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT name, country FROM radio_stations")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };

        // 24 (semis d'origine) + 25 (annuaire) = 49. Plancher, pas égalité :
        // ajouter une station vérifiée ne doit pas casser le test.
        assert!(
            stations.len() >= 49,
            "le catalogue livré est retombé à {} stations",
            stations.len()
        );

        let pays: std::collections::BTreeSet<&str> =
            stations.iter().map(|(_, p)| p.as_str()).collect();
        assert!(
            pays.len() >= 8,
            "le catalogue livré ne couvre que {} pays : {pays:?}",
            pays.len()
        );
        // Le symptôme exact du ticket : tout était `France`.
        let hors_france = stations.iter().filter(|(_, p)| p != "France").count();
        assert!(
            hors_france >= 17,
            "seulement {hors_france} stations hors de France dans le catalogue livré"
        );

        // Et la station par laquelle le ticket est arrivé (Belkadi Yacine puis
        // Bilou, fil forum 1506) : une recherche « paradise » ne rendait rien
        // parce que le catalogue ne la contenait pas.
        assert!(
            stations
                .iter()
                .any(|(n, _)| n.to_lowercase().contains("paradise")),
            "Radio Paradise manque toujours au catalogue livré"
        );

        // Le vocabulaire de `country` est celui du semis d'origine, en
        // français : mélanger « France » et « FR » dans la même colonne rendrait
        // la recherche par pays (`radio_repo::search`) et toute facette
        // illisibles.
        for (name, country) in &stations {
            assert!(
                country.len() > 2,
                "station « {name} » : pays donné en code ISO (« {country} ») alors que le \
                 catalogue livré l'écrit en toutes lettres"
            );
        }
    }

    /// Issue #1960 : les stations mortes que l'utilisateur tient de l'annuaire
    /// et non du semis. Celles qui ont une adresse de remplacement vérifiée
    /// sont repointées ; les autres partent.
    #[test]
    fn les_radios_mortes_de_l_annuaire_sont_reparees() {
        // (URL morte, URL vivante attendue) — sondées le 2026-08-28.
        const REMPLACEES: [(&str, &str); 3] = [
            (
                "https://stream.wbgo.org/wbgo-hd",
                "https://ais-sa8.cdnstream1.com/3630_128.mp3",
            ),
            (
                "https://online.jamminvibezradio.com/listen/reggae/live.flac",
                "https://radio.jamminvibezonline.ca/listen/reggae/stream.aac",
            ),
            (
                "https://online.jamminvibezradio.com/listen/oldies/live.flac",
                "https://radio.jamminvibezonline.ca/listen/oldies/stream.aac",
            ),
        ];
        const RETIREES: [&str; 2] = [
            "http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three",
            "https://online.jamminvibezradio.com/listen/caribbean/live.flac",
        ];

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let compter = |url: &str| {
            let conn = db.connection().lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM radio_stations WHERE url = ?1",
                [url],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };

        // Une bibliothèque neuve ne porte aucune des URL MORTES : ni le semis
        // d'origine (33) ni celui de l'annuaire (90) ne les posent — c'est
        // précisément ce que garde `le_semis_ne_pose_rien_qu_une_migration_supprime`.
        for (morte, _) in REMPLACEES {
            assert_eq!(compter(morte), 0, "{morte} semée sur une base neuve");
        }
        for morte in RETIREES {
            assert_eq!(compter(morte), 0, "{morte} semée sur une base neuve");
        }

        // Les URL de REMPLACEMENT, elles, sont désormais semées par la
        // migration 90 (#2119) : ces trois stations font partie du catalogue
        // livré, avec l'adresse vérifiée. On mesure donc en DELTA à partir de
        // ce plancher, sans quoi ce test comparerait à un passé révolu.
        let plancher: Vec<i64> = REMPLACEES.iter().map(|(_, v)| compter(v)).collect();

        // Bibliothèque existante : l'utilisateur les a ajoutées depuis
        // l'annuaire, avec le nom et le drapeau favori que le bouton pose.
        {
            let conn = db.connection().lock().unwrap();
            for (morte, _) in REMPLACEES {
                conn.execute(
                    "INSERT INTO radio_stations (name, url, genre, country, is_favorite) VALUES ('depuis-annuaire', ?1, 'g', 'XX', 1)",
                    [morte],
                )
                .unwrap();
            }
            for morte in RETIREES {
                conn.execute(
                    "INSERT INTO radio_stations (name, url, genre, country, is_favorite) VALUES ('depuis-annuaire', ?1, 'g', 'XX', 1)",
                    [morte],
                )
                .unwrap();
            }
            // Témoin : une station que l'utilisateur a lui-même repointée. La
            // migration cible l'URL, pas le nom : elle doit survivre.
            conn.execute(
                "INSERT INTO radio_stations (name, url, genre, country) VALUES ('BBC Radio 3', 'https://exemple.invalid/temoin', 'g', 'XX')",
                [],
            )
            .unwrap();
        }
        let total_avant = {
            let conn = db.connection().lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM radio_stations", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };

        // Rejouer la migration est ce que fait la mise à jour d'une base restée
        // sous la version 86 ; on la force ici, celle-ci étant déjà à jour.
        {
            let conn = db.connection().lock().unwrap();
            let m = MIGRATIONS.iter().find(|m| m.version == 86).unwrap();
            conn.execute_batch(m.up).unwrap();
            // Idempotent : un second passage est un non-événement.
            conn.execute_batch(m.up).unwrap();
        }

        for (i, (morte, vivante)) in REMPLACEES.iter().enumerate() {
            assert_eq!(compter(morte), 0, "l'URL morte {morte} est restée");
            assert_eq!(
                compter(vivante),
                plancher[i] + 1,
                "l'URL de remplacement {vivante} n'a pas été posée"
            );
        }
        for morte in RETIREES {
            assert_eq!(compter(morte), 0, "la station morte {morte} est restée");
        }

        // Le remplacement garde la ligne — nom, genre, favori — et ne la
        // recrée pas : seule l'adresse change.
        {
            let conn = db.connection().lock().unwrap();
            // `name = 'depuis-annuaire'` désigne LA ligne de la fixture : la
            // station homonyme semée par la migration 90 porte la même adresse
            // et n'a évidemment pas le drapeau favori de l'utilisateur.
            let favori: i64 = conn
                .query_row(
                    "SELECT is_favorite FROM radio_stations WHERE url = ?1 AND name = 'depuis-annuaire'",
                    ["https://ais-sa8.cdnstream1.com/3630_128.mp3"],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(favori, 1, "le remplacement a perdu le drapeau favori");
        }
        assert_eq!(
            compter("https://exemple.invalid/temoin"),
            1,
            "la migration 86 a frappé sur le nom au lieu de l'URL"
        );
        let total_apres = {
            let conn = db.connection().lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM radio_stations", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(
            total_apres,
            total_avant - RETIREES.len() as i64,
            "la migration 86 a retiré autre chose que les deux stations sans remplacement"
        );
    }

    #[test]
    fn renumber_queue_positions_sql_unifies_position_space() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        // Pre-unification layout: local rows at 0,1 and streaming rows at 0,1
        // (overlapping) for one zone.
        let conn = db.connection().lock().unwrap();
        // Isolated SQL test: no real zones/tracks, so relax FK enforcement.
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(
            "INSERT INTO queue_items (zone_id, position, track_id, source) VALUES (1, 0, 101, 'local');
             INSERT INTO queue_items (zone_id, position, track_id, source) VALUES (1, 1, 102, 'local');
             INSERT INTO queue_items (zone_id, position, source_id, source) VALUES (1, 0, 'q1', 'qobuz');
             INSERT INTO queue_items (zone_id, position, source_id, source) VALUES (1, 1, 'q2', 'qobuz');",
        )
        .unwrap();
        // The v53 renumber SQL: streaming rows shift to L..L+S-1.
        conn.execute_batch(RENUMBER_QUEUE_POSITIONS_SQL).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT track_id, position FROM queue_items WHERE zone_id = 1 ORDER BY position",
            )
            .unwrap();
        let rows: Vec<(Option<i64>, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        // Local stays 0,1; streaming now 2,3 → one contiguous space, no collisions.
        assert_eq!(
            rows,
            vec![(Some(101), 0), (Some(102), 1), (None, 2), (None, 3)]
        );
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

    #[test]
    fn default_smart_collections_are_not_duplicated() {
        // Regression for migration 47 re-seeding without a UNIQUE guard: the
        // default collections must appear exactly once after all migrations.
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let conn = db.connection().lock().unwrap();
        let max_dupe: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(c), 0) FROM \
                 (SELECT COUNT(*) c FROM smart_collections GROUP BY name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(max_dupe, 1, "smart_collections has duplicate names");

        // The UNIQUE index must reject a re-seed attempt (OR IGNORE => no-op).
        drop(conn);
        db.execute_batch(
            "INSERT OR IGNORE INTO smart_collections (name, rules) VALUES ('🎷 Jazz', '[]');",
        )
        .unwrap();
        let conn = db.connection().lock().unwrap();
        let jazz: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM smart_collections WHERE name = '🎷 Jazz'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(jazz, 1, "re-seed must not duplicate an existing collection");
    }

    /// Migration 87 (#1865) — le prédicat doit viser JUSTE.
    ///
    /// Elle retire les témoins de passe qui ne recouvrent aucun résultat sur
    /// les chemins non-ASCII. Trop large, elle relancerait l'analyse de toute
    /// une bibliothèque ; trop étroite, elle ne rattraperait rien et le
    /// correctif resterait sans effet sur les bases déjà balayées.
    ///
    /// Le SQL est relu depuis `MIGRATIONS` et rejoué APRÈS coup : cela vérifie
    /// du même geste que la migration est bien **idempotente** — un DELETE
    /// rejoué ne trouve plus rien, mais surtout il ne doit pas se mettre à
    /// mordre sur des lignes qu'il avait épargnées.
    #[test]
    fn la_migration_87_ne_retire_que_les_temoins_sans_resultat_sur_chemin_accentue() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        let sql = MIGRATIONS
            .iter()
            .find(|m| m.version == 87)
            .expect("migration 87 absente")
            .up;

        // id, chemin, a un gain / un vecteur ?, oversized ?
        // Les chemins accentués sont écrits en échappements explicites : un
        // éditeur qui recomposerait le littéral ferait passer le test sans
        // qu'il compare quoi que ce soit.
        let fixtures: &[(i64, &str, bool, bool)] = &[
            // 1 — accentué, aucun gain, non oversized  → témoin RETIRÉ
            (1, "/m/Bj\u{00f6}rk/01.flac", false, false),
            // 2 — accentué mais un gain existe          → témoin GARDÉ
            (2, "/m/\u{00c9}tienne/02.flac", true, false),
            // 3 — ASCII, aucun gain                     → témoin GARDÉ
            (3, "/m/Gramophone/03.flac", false, false),
            // 4 — accentué, aucun gain, mais oversized  → témoin GARDÉ
            (4, "/m/N\u{00fa}\u{00f1}ez/04.flac", false, true),
        ];

        db.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
        for (id, chemin, avec_resultat, oversized) in fixtures {
            db.execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path) \
                 VALUES (?, 'T', 1, 1, ?)",
                &[id, chemin],
            )
            .unwrap();
            db.execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'rg_analyzed', '1')",
                &[id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO track_metadata (track_id, key, value) \
                 VALUES (?, 'audio_embed_analyzed', 'clap')",
                &[id],
            )
            .unwrap();
            if *avec_resultat {
                db.execute(
                    "INSERT INTO track_metadata (track_id, key, value) \
                     VALUES (?, 'rg_track_gain', '-6.00 dB')",
                    &[id],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at) \
                     VALUES (?, 'clap', X'00', 0)",
                    &[id],
                )
                .unwrap();
            }
            if *oversized {
                db.execute(
                    "INSERT INTO track_metadata (track_id, key, value) \
                     VALUES (?, 'rg_skipped_oversized', '1')",
                    &[id],
                )
                .unwrap();
            }
        }

        let porte = |cle: &str| -> Vec<i64> {
            let conn = db.connection().lock().unwrap();
            let mut st = conn
                .prepare("SELECT track_id FROM track_metadata WHERE key = ?1 ORDER BY track_id")
                .unwrap();
            let v: Vec<i64> = st
                .query_map([cle], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            v
        };

        assert_eq!(porte("rg_analyzed"), vec![1, 2, 3, 4], "etat de depart");
        assert_eq!(porte("audio_embed_analyzed"), vec![1, 2, 3, 4]);

        db.execute_batch(sql).unwrap();

        // Seule la piste 1 remplit les trois conditions.
        assert_eq!(
            porte("rg_analyzed"),
            vec![2, 3, 4],
            "seul le temoin accentue et sans gain doit sauter ; \
             un gain existant (2), un chemin ASCII (3) et un refus pour \
             taille (4) sont des decisions valables, pas des ENOENT"
        );
        // Cote acoustique, la 4 saute AUSSI : `rg_skipped_oversized` est un
        // refus propre a ReplayGain (une estimation de memoire de decodage) et
        // ne dit rien de l'empreinte, qui ne lit que dix secondes. Les deux
        // regles ne sont donc volontairement pas les memes.
        assert_eq!(
            porte("audio_embed_analyzed"),
            vec![2, 3],
            "la 2 a un vecteur, la 3 est ASCII ; la 4 n'a pas de vecteur et son \
             chemin est accentue — `rg_skipped_oversized` ne la protege pas ici"
        );

        // Rejeu : idempotente, et surtout elle ne mord pas plus loin.
        db.execute_batch(sql).unwrap();
        assert_eq!(porte("rg_analyzed"), vec![2, 3, 4], "rejeu idempotent");
        assert_eq!(porte("audio_embed_analyzed"), vec![2, 3]);
    }

    // The PG numeric-type heal chain (#1220): the migration list must stay
    // contiguous and 1-based so run_pg_migrations applies every step, and the
    // numeric-column-type heal migrations (010/011/013/036) must all be present — a
    // gap or a missing heal would leave a data-migrated DB with TEXT numeric
    // columns and re-break force-scan album resolution (`operator does not
    // exist: text = bigint`). (012 heals integer id columns, a sibling fix.)
    // Runs without a live PG.
    #[cfg(feature = "postgres")]
    #[test]
    fn pg_migrations_are_contiguous_and_include_numeric_heals() {
        for (idx, &(version, _, _)) in PG_MIGRATIONS.iter().enumerate() {
            assert_eq!(
                version as usize,
                idx + 1,
                "PG_MIGRATIONS must be contiguous and 1-based"
            );
        }
        // Ce nombre se met a jour A LA MAIN, et c'est voulu : il oblige a
        // constater qu'une migration PG a ete ajoutee. Ajouter le fichier SQL
        // sans toucher a cette ligne fait echouer le job « Test (PostgreSQL) »,
        // qui est le seul a executer ce test — la feature `postgres` n'est pas
        // dans le jeu par defaut.
        // 42 : la 41 (`hidden_items`, #1391) était déjà enregistrée sans que
        // cette ligne soit remontée — ce test ne tourne que dans le job PG,
        // qui n'exécute pas `-p tune-core`. On la remonte donc de 40 à 42 d'un
        // coup, en constatant les DEUX ajouts.
        // 43 : `album_distinct_pairs` (#1276), jumelle de la SQLite 91.
        // 44 : `ignored_devices` (#1280), jumelle de la SQLite 92. Les numéros
        // 42 puis 43 ont été pris tour à tour par le semis de radios (#2119)
        // et par `album_distinct_pairs` (#1276), tous deux fusionnés pendant
        // que cette branche passait ses portes — d'où le renumérotage, fait
        // AVANT qu'aucune base ne voie l'ancien numéro.
        // 45 : `resserrer_folk_dans_world_music` (#1426), jumelle de la
        // SQLite 93. Migration de DONNÉES : sans jumelle PG, le préréglage
        // « World Music » garderait « contient folk » sur tout le parc
        // PostgreSQL.
        // 46 : `listen_history_rang_dans_le_contexte` (#2441). ⚠️ Cette
        // migration a été ajoutée SANS remonter la borne ci-dessous : la
        // branche était donc déjà ROUGE sur ce test avant #2860. Personne ne
        // l'a vu parce que « Test (PostgreSQL) » est SAUTÉ sur une PR vers
        // `batch/*` ou `rc/*` sans l'étiquette `ci:full` — c'est cette
        // PR-ci, qui la porte, qui a réveillé le test.
        // 47 : `listen_history_album_id_bigint` (#2860). `album_id` était
        // TEXT contre `albums.id` BIGINT : la jointure de « Continuer
        // l'écoute » rendait `operator does not exist: text = bigint`, avalé
        // par `unwrap_or_default()`, donc section vide sur tout le parc
        // PostgreSQL.
        // 48 : `zones_volume_a_virgule` (#2886). `zones.volume` était un
        // INTEGER : sous 0,005 linéaire (−46,0205999133 dB) la valeur
        // persistée tombait à 0 et la zone se rallumait MUETTE. PAS de jumelle
        // SQLite — voir le commentaire de l'entrée 48 dans PG_MIGRATIONS.
        assert_eq!(pg_latest_version(), 48, "latest PG migration must be 48");
        for wanted in [10, 11, 13, 36] {
            assert!(
                PG_MIGRATIONS.iter().any(|&(v, _, _)| v == wanted),
                "numeric-type heal migration {wanted} must be registered"
            );
        }
        assert!(
            PG_MIGRATIONS
                .iter()
                .any(|&(v, name, _)| v == 36 && name == "bookmarks_position_bigint"),
            "#2468 repair must stay registered as migration 036"
        );
    }

    /// #1440 — cas RÉEL : l'anthologie « OUF », douze lignes issues de douze
    /// dossiers d'artistes, se replie en une seule.
    #[test]
    fn scattered_compilation_rows_are_folded_into_one() {
        const TITLE: &str = "OUF L'anthologie Souterraine 2015-2017";
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let ids: Vec<i64> = ["Corte Real", "Alligator", "Oulane"]
            .iter()
            .enumerate()
            .map(|(i, artiste)| {
                let folder = format!("/mnt/recordings_usb/Qobuz/{artiste}/{TITLE}");
                let conn = db.connection().lock().unwrap();
                conn.execute(
                    "INSERT INTO albums (title, folder_path) VALUES (?, ?)",
                    rusqlite::params![TITLE, folder],
                )
                .unwrap();
                let id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO tracks (title, album_id, track_number, file_path) VALUES (?, ?, ?, ?)",
                    rusqlite::params![format!("piste {i}"), id, (i as i64) + 1, format!("{folder}/0{}.flac", i + 1)],
                )
                .unwrap();
                id
            })
            .collect();

        super::merge_scattered_compilations(&db);

        let conn = db.connection().lock().unwrap();
        let restants: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE title = ?",
                [TITLE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(restants, 1, "les trois éclats doivent tenir en un album");
        let sur_le_survivant: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracks WHERE album_id = ?",
                [ids[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sur_le_survivant, 3, "les pistes suivent l'album conservé");
    }

    /// #1440 + #1444 — cas RÉEL : « ALLOPOP », quatre volumes portant le MÊME
    /// titre, éclatés en 41 dossiers sous `Qobuz/`. Seule la pochette déposée
    /// dans chaque dossier les sépare ; sans elle, les numéros de piste se
    /// chevauchent et la grappe entière serait abandonnée.
    #[test]
    fn several_volumes_sharing_a_title_are_split_by_their_cover() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Trois artistes du volume 1 (pistes 1, 2, 3) et deux du volume 2
        // (pistes 1, 2) : les numéros se chevauchent d'un volume à l'autre.
        let volumes: [(u32, &[(&str, i64)]); 2] = [
            (1, &[("Diane", 1), ("Gatien", 2), ("Loup Blaster", 3)]),
            (2, &[("Tristan Savoie", 1), ("Ma Fraisse", 2)]),
        ];
        for (volume, membres) in volumes {
            for (artiste, piste) in membres {
                let folder = dir.path().join(artiste).join("ALLOPOP");
                std::fs::create_dir_all(&folder).unwrap();
                // Chaque dossier reçoit la pochette de son volume RÉ-ENCODÉE :
                // Qobuz ne livre pas deux fois le même fichier, et c'est
                // précisément ce qui trompait la comparaison par octets.
                std::fs::write(
                    folder.join("cover.jpg"),
                    crate::scanner::compilation::pochette_de_test(
                        volume,
                        96,
                        60 + (*piste as u8) * 8,
                    ),
                )
                .unwrap();
                let folder = folder.to_str().unwrap().to_string();
                let conn = db.connection().lock().unwrap();
                conn.execute(
                    "INSERT INTO albums (title, folder_path) VALUES ('ALLOPOP', ?)",
                    rusqlite::params![folder],
                )
                .unwrap();
                let id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO tracks (title, album_id, track_number, file_path) VALUES (?, ?, ?, ?)",
                    rusqlite::params![artiste, id, piste, format!("{folder}/0{piste}.flac")],
                )
                .unwrap();
            }
        }

        super::merge_scattered_compilations(&db);

        let conn = db.connection().lock().unwrap();
        let restants: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE title = 'ALLOPOP'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            restants, 2,
            "cinq dossiers, deux pochettes ⇒ deux albums — ni un, ni cinq"
        );
        let pistes: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pistes, 5, "aucune piste perdue au passage");
    }

    /// LE DÉGÂT DÉJÀ FAIT par les 0.9.66/0.9.67 : quatre volumes écrasés en UN
    /// SEUL album (#1470). La migration de rattrapage sait recoller ce qui est
    /// éparpillé — sait-elle SÉPARER ce qui a été fusionné à tort ?
    ///
    /// Reproduit l'état de .18 : un album, des pistes venues de dossiers aux
    /// pochettes différentes, et des numéros en double.
    #[test]
    fn an_album_wrongly_merged_is_split_back_by_cover() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();

        // Un seul album, quatre volumes dedans, chaque numéro en double.
        let album_id = {
            let conn = db.connection().lock().unwrap();
            let premier = dir.path().join("Diane").join("ALLOPOP");
            std::fs::create_dir_all(&premier).unwrap();
            std::fs::write(
                premier.join("cover.jpg"),
                crate::scanner::compilation::pochette_de_test(0, 96, 90),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO albums (title, folder_path) VALUES ('ALLOPOP', ?)",
                rusqlite::params![premier.to_string_lossy()],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        for (vol, artiste, num) in [
            (0usize, "Diane", 1),
            (1, "Tristan", 1),
            (2, "Nina", 1),
            (3, "Oscar", 1),
        ] {
            let d = dir.path().join(artiste).join("ALLOPOP");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("cover.jpg"),
                crate::scanner::compilation::pochette_de_test(vol as u32, 96, 90),
            )
            .unwrap();
            let conn = db.connection().lock().unwrap();
            conn.execute(
                "INSERT INTO tracks (title, album_id, track_number, file_path) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    artiste,
                    album_id,
                    num,
                    format!("{}/01.flac", d.to_string_lossy())
                ],
            )
            .unwrap();
        }

        super::merge_scattered_compilations(&db);

        let conn = db.connection().lock().unwrap();
        let albums: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE title='ALLOPOP'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            albums, 4,
            "un album fusionné à tort doit être redécoupé selon les pochettes"
        );
    }

    /// #1440 — cas RÉEL inverse : deux « Greatest Hits » d'artistes différents,
    /// tous deux numérotés à partir de 1. La collision protège.
    #[test]
    fn homonymous_albums_are_never_folded() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        for (artiste, dossier) in [
            ("Pat Benatar", "/data/music/P/Pat Benatar/Greatest Hits"),
            ("Police", "/data/music/P/Police/Greatest Hits"),
        ] {
            let conn = db.connection().lock().unwrap();
            conn.execute(
                "INSERT INTO albums (title, folder_path) VALUES (?, ?)",
                rusqlite::params!["Greatest Hits", dossier],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO tracks (title, album_id, track_number, file_path) VALUES (?, ?, 1, ?)",
                rusqlite::params![artiste, id, format!("{dossier}/01.flac")],
            )
            .unwrap();
        }

        super::merge_scattered_compilations(&db);

        let conn = db.connection().lock().unwrap();
        let restants: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE title = 'Greatest Hits'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(restants, 2, "deux disques homonymes restent deux albums");
    }

    /// #1612 — la casse de `format` est repliée sur les données déjà écrites.
    #[test]
    fn la_migration_80_replie_la_casse_de_format() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        {
            let conn = db.connection().lock().unwrap();
            conn.execute_batch(
                "INSERT INTO albums (title, format) VALUES \
                   ('Un', 'DSD'), ('Deux', 'dsd'), ('Trois', 'Dsd'), \
                   ('Quatre', ' flac '), ('Cinq', NULL);",
            )
            .unwrap();
        }

        // Rejouer le seul `up:` de la 78 : le runner l'a déjà passé sur une base
        // neuve, avant que ces lignes n'existent.
        // Le SQL vit dans une constante, pas dans `up:` : il est gardé par
        // `has_column` dans le bloc de version, parce qu'une base ancienne peut
        // ne pas encore avoir la colonne `format` — et un UPDATE qui échoue
        // casse TOUT le runner.
        db.connection()
            .lock()
            .unwrap()
            .execute_batch(SQL_FORMAT_LOWERCASE)
            .unwrap();

        let formats: Vec<String> = {
            let conn = db.connection().lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT format FROM albums WHERE format IS NOT NULL ORDER BY format",
                )
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        // Trois casses de « DSD » repliées en une, et l'espace de fin retiré —
        // un blanc produit exactement le même doublon invisible à l'écran.
        assert_eq!(formats, vec!["dsd".to_string(), "flac".to_string()]);
    }

    /// #1612 — chaque piste DSD retrouve son conteneur, lu sur son chemin.
    #[test]
    fn la_migration_81_rend_son_conteneur_a_chaque_piste_dsd() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();

        {
            let conn = db.connection().lock().unwrap();
            conn.execute_batch(
                "INSERT INTO albums (id, title, format) VALUES \
                   (1, 'Tout en DSF', 'dsd'), \
                   (2, 'Tout en DFF', 'dsd'), \
                   (3, 'Melange',     'dsd'); \
                 INSERT INTO tracks (album_id, title, file_path, format) VALUES \
                   (1, 'a', '/m/a.dsf', 'dsd'), \
                   (1, 'b', '/m/B.DSF', 'dsd'), \
                   (2, 'c', '/m/c.dff', 'dsd'), \
                   (3, 'd', '/m/d.dsf', 'dsd'), \
                   (3, 'e', '/m/e.dff', 'dsd');",
            )
            .unwrap();
        }

        {
            let conn = db.connection().lock().unwrap();
            conn.execute_batch(SQL_FORMAT_CONTENEUR).unwrap();
            // Idempotence : un second passage ne doit rien changer de plus.
            conn.execute_batch(SQL_FORMAT_CONTENEUR).unwrap();
        }

        let lire = |sql: &str| -> Vec<String> {
            let conn = db.connection().lock().unwrap();
            let mut stmt = conn.prepare(sql).unwrap();
            let v: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            v
        };

        // La casse du chemin ne doit pas décider : `.DSF` compte comme `.dsf`.
        assert_eq!(
            lire("SELECT format FROM tracks ORDER BY title"),
            vec!["dsf", "dsf", "dff", "dsf", "dff"]
        );

        // Un album homogène prend le conteneur de ses pistes ; un album qui
        // mélange les deux garde « dsd », qui reste vrai et reste reconnu.
        assert_eq!(
            lire("SELECT format FROM albums ORDER BY id"),
            vec!["dsf", "dff", "dsd"]
        );
    }

    /// Toute colonne posée côté SQLite existe aussi côté PostgreSQL — dans une
    /// MIGRATION, pas seulement dans le schéma neuf.
    ///
    /// La doctrine dit « trois endroits » : `CORE_SCHEMA` SQLite, migration
    /// SQLite, schéma PG. Il en faut **quatre** — le schéma PG neuf
    /// (`pg_migrate.rs`) ET la migration PG pour les bases existantes. C'est
    /// la quatrième qui manquait aux trois colonnes CUE : posées par la
    /// migration SQLite 76 en août, elles n'ont jamais atteint une base
    /// PostgreSQL déjà créée, et ne l'auraient jamais fait (#2111).
    ///
    /// Le défaut serait resté invisible jusqu'au jour où du code les aurait
    /// écrites — et l'échec se serait alors lu comme un défaut du CUE, pas
    /// comme une migration manquante.
    ///
    /// Ce test lit les SOURCES, comme `network_mounts_n_a_qu_une_definition` :
    /// il vaut donc quel que soit le jeu de features compilé.
    #[test]
    fn toute_colonne_sqlite_a_sa_migration_postgres() {
        let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
        let sqlite = fs::read_to_string(racine.join("src/db/migrations.rs")).unwrap();

        // Les colonnes ajoutées par `add_column_if_missing` côté SQLite.
        let mut colonnes: Vec<String> = Vec::new();
        for l in sqlite.lines() {
            let Some(i) = l.find("add_column_if_missing(db, \"") else {
                continue;
            };
            let reste = &l[i..];
            let champs: Vec<&str> = reste.split('"').collect();
            // add_column_if_missing(db, "table", "colonne", "type")
            if champs.len() >= 4 {
                colonnes.push(champs[3].to_string());
            }
        }
        assert!(
            colonnes.len() > 20,
            "aucune colonne trouvée ({}) — le motif d'appel a changé, ce test ne garde plus rien",
            colonnes.len()
        );

        // Tout le SQL PostgreSQL, migrations numérotées SEULEMENT.
        //
        // `pg_migrate.rs` est délibérément EXCLU : c'est le schéma neuf, et
        // c'est précisément là que les colonnes CUE se cachaient tout en
        // manquant aux bases existantes.
        let dossier = racine.join("migrations/postgres");
        let mut sql_pg = String::new();
        for e in fs::read_dir(&dossier).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "sql") {
                sql_pg.push_str(&fs::read_to_string(e.path()).unwrap_or_default());
            }
        }

        let manquantes: Vec<&String> = colonnes
            .iter()
            .filter(|c| !sql_pg.contains(c.as_str()))
            .collect();

        assert!(
            manquantes.is_empty(),
            "colonne(s) posée(s) côté SQLite et ABSENTE(S) des migrations PostgreSQL : {manquantes:?}\n\
             Une base PostgreSQL déjà créée ne les recevra JAMAIS — `CREATE TABLE` dans\n\
             `pg_migrate.rs` ne s'applique qu'à une base vide. Ajouter un fichier dans\n\
             `tune-core/migrations/postgres/` avec `ADD COLUMN IF NOT EXISTS` (#2111)."
        );
    }

    /// Les deux moteurs ont des listes SÉPARÉES : une correction de données
    /// écrite d'un seul côté ne répare qu'une moitié du parc (#1612).
    #[test]
    fn toute_migration_sqlite_de_donnees_a_sa_jumelle_postgres() {
        assert!(
            MIGRATIONS.iter().any(|m| m.name == "format_lowercase"),
            "la migration SQLite `format_lowercase` a disparu"
        );

        // `PG_MIGRATIONS` vit derrière `#[cfg(feature = "postgres")]`, donc le
        // gate par défaut ne la compile pas. On lit la SOURCE plutôt que la
        // constante — même approche que `network_mounts_n_a_qu_une_definition`,
        // et le garde-fou vaut alors quel que soit le jeu de features.
        let ce_fichier = include_str!("migrations.rs");
        for (fichier, nom) in [
            ("029_format_lowercase.sql", "format_lowercase"),
            ("030_format_conteneur_dsd.sql", "format_conteneur_dsd"),
            // #1426 — le préréglage « World Music » est arrivé sur PostgreSQL
            // par la bascule, jamais par un semis PG : sans jumelle, « contient
            // folk » y resterait pour toujours.
            (
                "045_resserrer_folk_dans_world_music.sql",
                "resserrer_folk_dans_world_music",
            ),
        ] {
            assert!(
                MIGRATIONS.iter().any(|m| m.name == nom),
                "la migration SQLite `{nom}` a disparu"
            );
            assert!(
                ce_fichier.contains(fichier),
                "`{nom}` existe côté SQLite mais n'est pas enregistrée dans \
                 PG_MIGRATIONS : les serveurs PostgreSQL (.15, .18, Docker) \
                 garderaient le défaut entier. Les deux listes sont SÉPARÉES — \
                 `run_migrations` ne prend qu'un `SqliteDb`."
            );
            let sql = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("migrations/postgres/{fichier}"));
            assert!(
                sql.exists(),
                "l'entrée PG_MIGRATIONS pointe sur un fichier absent : {}",
                sql.display()
            );
        }
    }

    /// Le registre des exécutions (#2080) doit exister sur les QUATRE chemins
    /// par lesquels une base arrive à la vie. Trois suffiraient à faire croire
    /// que c'est fait.
    ///
    /// Le chemin qu'on oublie est le troisième : une base créée par
    /// l'assistant SQLite → PostgreSQL enregistre `schema_version = 99` et ne
    /// rejoue **jamais** les scripts PG numérotés. Sur ces bases-là, la
    /// migration 039 ne s'appliquera pas — ni maintenant, ni jamais. C'est
    /// exactement la dérive documentée sur `lyrics_cache`.
    ///
    /// Ce test lit les SOURCES : il vaut donc quel que soit le jeu de features
    /// compilé, `PG_MIGRATIONS` vivant derrière `#[cfg(feature = "postgres")]`.
    #[test]
    fn le_registre_des_executions_existe_sur_les_quatre_chemins() {
        let racine = Path::new(env!("CARGO_MANIFEST_DIR"));

        // 1. Migration SQLite — les bases SQLite existantes ET neuves (le
        //    runner tourne à chaque démarrage).
        assert!(
            MIGRATIONS.iter().any(|m| m.name == "task_runs"),
            "la migration SQLite `task_runs` a disparu"
        );

        // 2. Migration PG numérotée — les bases PostgreSQL sur la piste
        //    numérotée (.15, .18, Docker).
        let ce_fichier = include_str!("migrations.rs");
        assert!(
            ce_fichier.contains("040_task_runs.sql"),
            "`task_runs` existe côté SQLite mais n'est pas enregistrée dans \
             PG_MIGRATIONS"
        );
        assert!(
            racine
                .join("migrations/postgres/040_task_runs.sql")
                .exists(),
            "l'entrée PG_MIGRATIONS pointe sur un fichier absent"
        );

        // 3. `PG_FULL_SCHEMA` — une base montée d'un bloc par la bascule
        //    SQLite → PostgreSQL, qui ne rejouera aucun script numéroté.
        let pg_neuf = fs::read_to_string(racine.join("src/db/pg_migrate.rs")).unwrap();
        assert!(
            pg_neuf.contains("CREATE TABLE IF NOT EXISTS task_runs"),
            "`task_runs` manque à PG_FULL_SCHEMA : une base créée par la \
             bascule SQLite → PostgreSQL porte schema_version 99 et ne \
             recevrait JAMAIS la migration 040"
        );

        // 4. `ENSURE_TABLES` — le rattrapage rejoué à CHAQUE démarrage, seul
        //    filet pour une base déjà convertie AVANT cette version.
        let ensure = fs::read_to_string(racine.join("src/db/postgres.rs")).unwrap();
        assert!(
            ensure.contains("CREATE TABLE IF NOT EXISTS task_runs"),
            "`task_runs` manque à ENSURE_TABLES : les bases PostgreSQL déjà \
             converties (schema_version 99) resteraient sans registre pour \
             toujours"
        );
    }
}

#[cfg(test)]
mod schema_unique_tests {
    use std::fs;
    use std::path::Path;

    /// Garde-fou : une seule définition de `network_mounts` dans tout le code.
    ///
    /// #1692 — il en existait TROIS, dont deux concurrentes portant le même nom
    /// avec des colonnes différentes :
    ///
    ///   - `routes/network.rs` écrivait `mount_type/server/share/…/active` ;
    ///   - `mount_manager.rs` déclarait `host/share_name/…/auto_mount/status`.
    ///
    /// Comme les deux disaient `CREATE TABLE IF NOT EXISTS`, celle qui passait
    /// la première gagnait — et l'autre chemin lisait ensuite une table dont les
    /// colonnes n'existaient pas. `mount_manager.rs` n'étant construit nulle
    /// part hors tests, le piège dormait : il se serait réveillé au premier
    /// appelant. Un correctif du remontage avait d'ailleurs déjà été écrit
    /// contre la mauvaise table, puis annulé.
    ///
    /// Le module mort a été supprimé. Ce test empêche qu'une seconde définition
    /// réapparaisse sans que personne ne le voie.
    #[test]
    fn network_mounts_n_a_qu_une_definition_par_moteur() {
        let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut declarations = Vec::new();

        for caisse in ["tune-core/src", "tune-server/src", "plugins"] {
            let base = racine.join(caisse);
            if !base.exists() {
                continue;
            }
            let mut piles = vec![base];
            while let Some(dir) = piles.pop() {
                let Ok(entrees) = fs::read_dir(&dir) else {
                    continue;
                };
                for e in entrees.flatten() {
                    let chemin = e.path();
                    if chemin.is_dir() {
                        piles.push(chemin);
                    } else if chemin.extension().is_some_and(|x| x == "rs") {
                        let Ok(texte) = fs::read_to_string(&chemin) else {
                            continue;
                        };
                        for ligne in texte.lines() {
                            let l = ligne.to_lowercase();
                            if l.contains("create table") && l.contains("network_mounts") {
                                declarations.push(
                                    chemin
                                        .strip_prefix(&racine)
                                        .unwrap_or(&chemin)
                                        .display()
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
            }
        }

        declarations.sort();
        declarations.dedup();

        // Une par moteur de base : SQLite (migrations.rs) et PostgreSQL
        // (pg_migrate.rs). Toute autre est un doublon.
        let attendues = [
            "tune-core/src/db/migrations.rs",
            "tune-core/src/db/pg_migrate.rs",
        ];
        let en_trop: Vec<&String> = declarations
            .iter()
            .filter(|d| !attendues.iter().any(|a| d.ends_with(a) || d == a))
            .collect();

        assert!(
            en_trop.is_empty(),
            "définition(s) concurrente(s) de `network_mounts` : {en_trop:?}\n\
             Une seule par moteur. Deux tables du même nom aux colonnes \
             différentes se masquent l'une l'autre via CREATE TABLE IF NOT \
             EXISTS, et le chemin perdant lit des colonnes inexistantes (#1692)."
        );

        assert!(
            declarations.len() >= 2,
            "aucune définition de `network_mounts` trouvée ({declarations:?}) — \
             le test ne garde plus rien : chemin de recherche ou nom de table changé ?"
        );
    }
}
