-- Audio embeddings (CLAP) for acoustic "sounds-like" radio: one 512-d vector per
-- track, computed in the background analysis pass (piggybacks the ReplayGain
-- decode) and stored as bytea. smart_radio ranks candidates by cosine
-- similarity. Empty until the opt-in pass runs — everything else is unchanged.

CREATE TABLE IF NOT EXISTS track_audio_embedding (
    track_id    BIGINT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    model       TEXT   NOT NULL,
    embedding   BYTEA  NOT NULL,
    analyzed_at BIGINT
);
