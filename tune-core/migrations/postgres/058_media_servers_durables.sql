-- 058_media_servers_durables.sql
--
-- #2219, phase 1 — le registre des serveurs multimédia devient DURABLE.
-- Jumelle PostgreSQL de la migration SQLite 101.
--
-- Le registre vivait entièrement en mémoire
-- (`Arc<Mutex<HashMap<String, MediaServerInfo>>>`, `tune-server/src/state.rs:86`)
-- et datait sa dernière observation avec un `Instant` `#[serde(skip)]`
-- (`discovery/ssdp.rs:146`) : rien ne survivait au redémarrage, et la date
-- n'était même pas représentable en absolu. Mesuré sur le `.18` le 13/09/2026,
-- `GET /api/v1/network/media-servers` rendait trois serveurs vus il y a
-- 84 194 s (23 h 23), tous `reachable: false`, et aucun moyen de dire si
-- c'était vrai.
--
-- Le modèle est `network_mounts`, et on en reprend la séparation qui a rendu
-- #1916 visible : `active` dit l'INTENTION, `last_state` / `absence_reason`
-- disent le CONSTAT.
--
-- `udn` EST la clef primaire : l'UDN est l'identité stable d'un appareil UPnP
-- (le port change, lui non — `discovery/redecouverte.rs:1-45`). Pas de colonne
-- `id`, donc pas de divergence AUTOINCREMENT / BIGSERIAL — même choix que
-- `streaming_item_tags` (PG 052), `favorite_facets` (PG 038) et `task_runs`.
--
-- `first_seen_at` n'est JAMAIS réécrit : un serveur qui revient ne perd pas son
-- histoire.
--
-- TEXT pour les dates des deux côtés, comme `zones.last_seen_at` (PG 050) :
-- rien à rattraper dans la parité de types.
--
-- Idempotent : CREATE TABLE IF NOT EXISTS est sûr à rejouer.

BEGIN;

CREATE TABLE IF NOT EXISTS media_servers (
    udn TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    manufacturer TEXT,
    model TEXT,
    device_type TEXT NOT NULL DEFAULT 'upnp_media_server',
    location TEXT NOT NULL DEFAULT '',
    content_directory_url TEXT,
    host TEXT,
    port INTEGER,
    max_age_secs INTEGER,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    last_state TEXT,
    absence_reason TEXT,
    created_at TEXT NOT NULL DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')
);

CREATE INDEX IF NOT EXISTS idx_media_servers_last_seen ON media_servers(last_seen_at);

INSERT INTO schema_version (version, name) VALUES (58, 'media_servers_durables')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
