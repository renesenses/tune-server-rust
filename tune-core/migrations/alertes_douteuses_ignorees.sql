CREATE TABLE IF NOT EXISTS alertes_douteuses_ignorees (
 portee TEXT NOT NULL,
 cle TEXT NOT NULL,
 titre TEXT NOT NULL DEFAULT '',
 raison TEXT NOT NULL,
 cible_id BIGINT,
 libelle TEXT,
 ignoree_le BIGINT NOT NULL,
 PRIMARY KEY (portee, cle, titre, raison)
)
