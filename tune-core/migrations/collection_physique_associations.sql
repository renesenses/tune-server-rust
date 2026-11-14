CREATE TABLE IF NOT EXISTS collection_physique_associations (
 release_id BIGINT NOT NULL PRIMARY KEY,
 album_id BIGINT NOT NULL,
 associe_le BIGINT NOT NULL
)
