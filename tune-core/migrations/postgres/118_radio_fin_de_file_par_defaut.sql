-- 118_radio_fin_de_file_par_defaut.sql
--
-- Forum, fil 2694 (Ilan, 12/11/2026) ; decision de Bertrand du 13/11 : en
-- rc12, cette migration activait la radio de fin de file sur toutes les zones
-- au reglage « desactive » et posait DEFAULT '1'. Jumelle de la SQLite 152.
--
-- #8974 (decision de Bertrand du 17/11) : retour au defaut DESACTIVE. Elle ne
-- fait PLUS rien : une base qui ne l'a pas encore jouee (rc11 ou avant ->
-- rc13) garde ses reglages d'avant la rc12. Elle marque un AUTRE nom que
-- l'ancienne : la 122 reconnait a l'ancien nom (`radio_fin_de_file_par_defaut`)
-- une base ou la bascule a vraiment eu lieu.

BEGIN;

INSERT INTO schema_version (version, name) VALUES (118, 'radio_fin_de_file_par_defaut_neutralisee')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
