-- 124_profil_administrateur_admin.sql
-- rc14 (décision du 26/11/2026) — le profil administrateur d'origine
-- s'appelle `admin` / `Admin`. Jumelle de la SQLite 158, même texte que
-- `profil_administrateur::SQL_RENOMMER_LE_PROFIL_D_ORIGINE`.
--
-- La 005 sème toujours le profil 1 en `default` : appliquée partout, elle ne
-- se modifie pas. Ici :
--   * le nom ne change que s'il vaut encore exactement `default` ;
--   * le nom affiché ne change que s'il vaut encore exactement `Default` ;
--   * rien ne change si un autre profil s'appelle déjà `admin` (le serveur
--     l'écrit au journal : `pg_profil_administrateur_garde_default`).
-- `default` reste accepté à la connexion comme alias du profil 1.
BEGIN;
UPDATE profiles
   SET username = 'admin',
       display_name = CASE WHEN display_name = 'Default' THEN 'Admin' ELSE display_name END
 WHERE id = 1 AND username = 'default'
   AND NOT EXISTS (SELECT 1 FROM profiles autre
                    WHERE autre.id <> 1 AND LOWER(autre.username) = 'admin');

INSERT INTO schema_version (version, name) VALUES (124, 'profil_administrateur_admin')
    ON CONFLICT (version) DO NOTHING;
COMMIT;
