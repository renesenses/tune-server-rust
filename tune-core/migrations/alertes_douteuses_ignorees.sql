CREATE TABLE IF NOT EXISTS alertes_douteuses_ignorees (
 portee TEXT NOT NULL,
 cible_id BIGINT NOT NULL,
 raison TEXT NOT NULL,
 ignoree_le BIGINT NOT NULL,
 PRIMARY KEY (portee, cible_id, raison)
)
