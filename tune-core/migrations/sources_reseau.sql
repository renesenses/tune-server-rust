-- #8189 — les sources réseau lues par le moteur lui-même, sans montage
-- (WebDAV, SMB, NFS : étapes 3, 4 et 6). Distincte de `network_mounts`, qui
-- décrit un montage SYSTÈME et range son mot de passe en clair.
--
-- `identifiants_scelles` : valeur scellée par le coffre partagé
-- (`{"v":1,nonce,ct}`), sous la clé de l'application hôte sur un téléphone.
-- Jamais de mot de passe en clair dans cette table.
-- `etat` : `nouvelle`, `a_jour`, `indexation`, `injoignable`,
-- `identifiants_refuses`, `non_prise_en_charge`.
-- `reprise_scan` : le point de reprise d'un parcours interrompu (JSON).
--
-- Même texte pour SQLite (149) et PostgreSQL (115).
CREATE TABLE IF NOT EXISTS sources_reseau (
 id TEXT PRIMARY KEY,
 type_source TEXT NOT NULL,
 nom TEXT NOT NULL,
 hote TEXT NOT NULL DEFAULT '',
 port BIGINT,
 partage TEXT NOT NULL DEFAULT '',
 racine TEXT NOT NULL DEFAULT '',
 identifiants_scelles TEXT,
 etat TEXT NOT NULL DEFAULT 'nouvelle',
 derniere_erreur TEXT,
 dernier_scan TEXT,
 reprise_scan TEXT,
 cree_le TEXT NOT NULL
)
