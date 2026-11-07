CREATE TABLE IF NOT EXISTS ecoutes_parentales (
 profile_id BIGINT NOT NULL,
 jour TEXT NOT NULL,
 secondes BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY (profile_id, jour)
)
