CREATE TABLE IF NOT EXISTS radio_titres_diffuses (
 ecoute_id BIGINT NOT NULL,
 diffuse_le BIGINT NOT NULL,
 zone_id BIGINT,
 flux TEXT,
 station TEXT NOT NULL DEFAULT '',
 titre TEXT NOT NULL,
 artiste TEXT,
 pochette TEXT,
 PRIMARY KEY (ecoute_id, diffuse_le)
)
