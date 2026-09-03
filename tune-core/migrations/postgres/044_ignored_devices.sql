-- ignored_devices : « ignorer cet appareil » pour qu'il cesse d'être proposé
-- (#1280, Alex Campbell puis Patatorz).
--
-- Jumelle PostgreSQL de la migration SQLite 92. Les deux listes sont
-- SEPAREES : écrite d'un seul côté, la table manquerait à tout le parc
-- PostgreSQL (.15, .18, Docker) et chaque annonce de découverte y rendrait
-- une erreur SQL, puisque le garde-fou la nomme.
--
-- Pourquoi une table et pas le masquage de zone existant (`zones.is_hidden`
-- + `hidden_zones_by_host`, #1281) : celui-ci ne porte que ce qui a DÉJÀ une
-- zone, et seulement pour les zones réseau. Il laisse l'appareil
-- s'enregistrer comme SORTIE — la découverte enregistre avant d'atteindre le
-- garde-fou de zone — donc proposé partout ; et un appareil dont la zone n'a
-- jamais été créée (`zone_auto_create` à false, TV filtrée, AirPlay 2 sans
-- démon) n'a aucune ligne à masquer.
--
-- Patron `hidden_items` (PG 041) : table SANS clé étrangère, instantané
-- d'identité figé à l'insertion. Le marqueur ne dépend d'aucune ligne
-- `zones` et survit à leur purge.
--
-- AUCUNE troisième notion d'identité : `mac` est celle de #2803
-- (AirPlay/RAOP, déjà persistée sur `zones.mac`), `host` + `name` est
-- exactement le couple de `hidden_zones_by_host`. Le NOM est exigé avec
-- l'hôte pour ne pas bloquer un appareil différent héritant de l'adresse par
-- le DHCP (leçon du ré-ancrage #1651).
--
-- PAS DE COLONNE `id` : `device_id` EST la clé primaire — même choix que
-- `favorite_facets` (PG 038), `task_runs` (PG 040) et `hidden_items`
-- (PG 041), pour éviter la divergence AUTOINCREMENT / BIGSERIAL de la
-- bascule SQLite -> PostgreSQL (#1706).
--
-- Aucun bloc de rattrapage de type ici, contrairement à 038 et 041 : toutes
-- les colonnes sont TEXT des deux côtés, donc `PG_FULL_SCHEMA` (qui lie les
-- valeurs SQLite en texte) et cette migration déclarent exactement la même
-- chose. Rien à réconcilier.
--
-- Idempotent : CREATE TABLE / CREATE INDEX IF NOT EXISTS.

CREATE TABLE IF NOT EXISTS ignored_devices (
    device_id TEXT PRIMARY KEY,
    mac TEXT NOT NULL DEFAULT '',
    host TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '',
    device_type TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE INDEX IF NOT EXISTS idx_ignored_devices_mac ON ignored_devices(mac);
CREATE INDEX IF NOT EXISTS idx_ignored_devices_host ON ignored_devices(host);
