BEGIN;
CREATE TABLE IF NOT EXISTS upnp_library_sources (
    source_key TEXT PRIMARY KEY,
    udn TEXT NOT NULL,
    container TEXT NOT NULL,
    state_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS upnp_library_members (
    source_key TEXT NOT NULL REFERENCES upnp_library_sources(source_key) ON DELETE CASCADE,
    track_id BIGINT NOT NULL,
    generation TEXT NOT NULL,
    PRIMARY KEY (source_key, track_id)
);

INSERT INTO schema_version (version, name) VALUES (59, 'upnp_library_sync') ON CONFLICT (version) DO NOTHING;
CREATE INDEX IF NOT EXISTS idx_upnp_library_members_track ON upnp_library_members(track_id);
COMMIT;
