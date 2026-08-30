-- 045_resserrer_folk_dans_world_music.sql
--
-- Jumelle PostgreSQL de la migration SQLite 93 (#1426, Jean Valjean, forum
-- « F5 obligatoire » : « Dans la Smart Collection "World Music" [il] n'a pas
-- les bons albums, c'est un peu mélangé (Folk, Folk Métal, Folk Rock) »).
--
-- Le préréglage livré porte `genre CONTIENT folk`, qui compile en
-- `t.genre LIKE '%folk%'` : « Folk Metal » et « Folk Rock » entrent dans la
-- collection par construction. Seul `folk` passe en égalité stricte ; `world`
-- et `ethnic` gardent « contient », où la sous-chaîne est utile (« World
-- Fusion », « Ethnic Jazz »).
--
-- PostgreSQL ne sème JAMAIS les collections intelligentes lui-même : ces bases
-- ont reçu le préréglage par la bascule SQLite → PostgreSQL, donc elles ont
-- besoin de la même correction que SQLite. Même situation que 018.
--
-- Gardé sur la chaîne de règles EXACTE du semis : une collection personnalisée
-- n'est jamais touchée, et la migration est idempotente par le même garde.

BEGIN;

UPDATE smart_collections
SET rules = '[{"field":"genre","operator":"contains","value":"world"},{"field":"genre","operator":"contains","value":"ethnic"},{"field":"genre","operator":"equals","value":"folk"}]'
WHERE name LIKE '%World%'
  AND rules = '[{"field":"genre","operator":"contains","value":"world"},{"field":"genre","operator":"contains","value":"ethnic"},{"field":"genre","operator":"contains","value":"folk"}]';

INSERT INTO schema_version (version, name) VALUES (45, 'resserrer_folk_dans_world_music')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
