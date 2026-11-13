CREATE TABLE IF NOT EXISTS podcast_episode_positions (
 zone_id BIGINT NOT NULL,
 episode_key TEXT NOT NULL,
 audio_url TEXT NOT NULL,
 position_ms BIGINT NOT NULL DEFAULT 0,
 duration_ms BIGINT NOT NULL DEFAULT 0,
 updated_at TEXT NOT NULL,
 PRIMARY KEY (zone_id, episode_key)
)
