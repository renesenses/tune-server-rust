CREATE TABLE IF NOT EXISTS politiques_parentales (
 profile_id BIGINT PRIMARY KEY,
 superviseur_id BIGINT NOT NULL,
 revision BIGINT NOT NULL,
 politique_json TEXT NOT NULL,
 maj_le TEXT NOT NULL
);
