CREATE TABLE IF NOT EXISTS jetons_de_renouvellement (
 id TEXT PRIMARY KEY,
 famille TEXT NOT NULL,
 profile_id BIGINT NOT NULL,
 empreinte_sha256 TEXT NOT NULL UNIQUE,
 empreinte_mot_de_passe TEXT NOT NULL,
 client TEXT,
 cree_le BIGINT NOT NULL,
 expire_le BIGINT NOT NULL,
 remplace_le BIGINT,
 revoque_le BIGINT
);
CREATE INDEX IF NOT EXISTS idx_jetons_de_renouvellement_famille ON jetons_de_renouvellement(famille);
CREATE INDEX IF NOT EXISTS idx_jetons_de_renouvellement_profil ON jetons_de_renouvellement(profile_id)
