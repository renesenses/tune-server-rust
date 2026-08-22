-- `podcast_subscriptions.source_id` : l'identifiant cote service de streaming.
--
-- La colonne existe depuis la migration SQLite v59, et dans PG_FULL_SCHEMA
-- (`pg_migrate.rs`). Mais PG_FULL_SCHEMA ne tourne QUE pendant la migration
-- unique SQLite -> PostgreSQL : une base PostgreSQL montee de zero ne le voit
-- jamais. Aucun script numerote ne l'ajoutait, et elle n'est ni dans
-- ENSURE_TABLES ni dans ENSURE_COLUMNS — les trois voies verifiees.
--
-- Consequence : `SELECT ... source_id FROM podcast_subscriptions`
-- (tune-server/src/routes/podcasts.rs) echouait sur toute installation
-- PostgreSQL fraiche. Meme famille que l'incident `queue_items` en production
-- sur le .15, documente dans `postgres.rs`.
--
-- Idempotent : sans danger a rejouer sur une base deja migree depuis SQLite,
-- ou la colonne est deja la.
ALTER TABLE podcast_subscriptions
    ADD COLUMN IF NOT EXISTS source_id TEXT;
