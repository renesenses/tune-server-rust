CREATE TABLE IF NOT EXISTS artistes_des_credits (
 credit_id BIGINT NOT NULL,
 artiste_id BIGINT NOT NULL,
 rang BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY (credit_id, artiste_id)
)
