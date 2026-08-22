-- #2111 : douze réglages de zone n'existaient pas côté PostgreSQL.
--
-- Trouvées par le garde-fou `toute_colonne_sqlite_a_sa_migration_postgres`,
-- ajouté dans la même PR — il les a signalées dès sa première exécution.
--
-- ## Ce que ça produisait
--
-- `zone_repo.rs` avale l'échec d'écriture quand la colonne manque :
--
--     Err(e) if e.contains("no such column") || e.contains("does not exist") => {
--         tracing::debug!(id, error = %e, "dlna_wav24_column_missing_ignoring_update");
--         Ok(())                                   // ← succès annoncé au client
--     }
--
-- Douze réglages font cela. Sur un serveur PostgreSQL : l'utilisateur coche la
-- case, **l'API répond que c'est enregistré**, la lecture rend la valeur par
-- défaut, et la seule trace est une ligne en `debug!` que personne ne lit.
--
-- C'est le symptôme exact de #1654 — « le réglage est accepté mais la lecture
-- reste en 16 bits » — pour tout utilisateur sur PostgreSQL.
--
-- ⚠️ `dlna_wav24` est le pire cas : elle manquait **aussi** à `pg_migrate.rs`,
-- donc elle n'existait sur AUCUNE base PostgreSQL, pas même neuve.
--
-- ## Les types
--
-- `INTEGER` côté SQLite devient `INTEGER` ici et non `BOOLEAN` : le code lit
-- ces colonnes en entier (`COALESCE(dlna_wav24, 0) … != 0`) et les écrit en
-- `enabled as i64`. Un `BOOLEAN` PostgreSQL casserait les deux sens. Le but est
-- que les deux moteurs répondent pareil, pas que chacun soit idiomatique.
--
-- `dlna_play_delay_ms` reste `INTEGER` : c'est un délai en millisecondes, borné
-- par nature, et `get_dlna_play_delay_ms` le relit en `u64` depuis un `i64`.

ALTER TABLE zones ADD COLUMN IF NOT EXISTS fixed_volume       INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS autoplay_enabled   INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS last_play_state    TEXT    DEFAULT 'stopped';
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dsd_mode           TEXT    DEFAULT 'auto';
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_native_flac   INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS alac_passthrough   INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS aac_passthrough    INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_lpcm          INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_cap_16bit     INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_wav24         INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS dlna_play_delay_ms INTEGER DEFAULT 0;
ALTER TABLE zones ADD COLUMN IF NOT EXISTS mac                TEXT;
