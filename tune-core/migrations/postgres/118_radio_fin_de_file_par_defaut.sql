-- 118_radio_fin_de_file_par_defaut.sql
--
-- Forum, fil 2694 (Ilan, 12/11/2026) ; decision de Bertrand du 13/11 : la
-- radio de fin de file (« continuer avec des titres similaires a la fin de la
-- file », comme Roon) est ACTIVEE par defaut. Jumelle de la migration SQLite
-- 152.
--
-- Une seule bascule des zones au reglage « desactive » (defaut historique
-- depuis la 046) vers « titres similaires » ('1') ; les modes nommes (#2271)
-- ne bougent pas. Les zones creees ensuite naissent a '1' (DEFAULT ci-dessous
-- et `ZoneRepo::create`). Desactivable par zone dans la file ou ses reglages.
-- Idempotent : rejouee, la condition ne trouve plus rien a changer qui n'ait
-- ete remis a '0' par l'utilisateur APRES — et la ligne de schema_version
-- l'empeche de rejouer.

BEGIN;

UPDATE zones SET autoplay_enabled = '1'
 WHERE autoplay_enabled IS NULL OR autoplay_enabled IN ('0', 'off');

ALTER TABLE zones ALTER COLUMN autoplay_enabled SET DEFAULT '1';

INSERT INTO schema_version (version, name) VALUES (118, 'radio_fin_de_file_par_defaut')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
