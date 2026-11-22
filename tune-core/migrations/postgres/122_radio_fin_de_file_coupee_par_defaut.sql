-- 122_radio_fin_de_file_coupee_par_defaut.sql
--
-- #8974 (decision de Bertrand du 17/11, apres les fils 2694 et 2887) : la radio
-- de fin de file redevient DESACTIVEE par defaut, pour les nouvelles zones et
-- les nouvelles installations. Jumelle de la SQLite 155.
--
-- Annule la bascule de la 118 de la rc12, et SEULEMENT la ou elle a eu lieu
-- (ligne schema_version 118 sous son ancien nom). Apres cette bascule, un '1'
-- (titres similaires) ne dit plus si l'utilisateur l'a choisi : la rc12 l'a
-- ecrit sur toutes les zones « desactive » et sur chaque zone creee. Rien ne
-- prouve le choix : ces zones reprennent le defaut (coupe). Les modes nommes
-- (#2271) ne sont jamais poses par defaut : ils restent. Le DEFAULT de la
-- colonne revient a '0' (celui de la 064).

BEGIN;

UPDATE zones SET autoplay_enabled = '0'
 WHERE autoplay_enabled IN ('1', 'similar')
   AND EXISTS (SELECT 1 FROM schema_version
                WHERE version = 118 AND name = 'radio_fin_de_file_par_defaut');

ALTER TABLE zones ALTER COLUMN autoplay_enabled SET DEFAULT '0';

INSERT INTO schema_version (version, name) VALUES (122, 'radio_fin_de_file_coupee_par_defaut')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
