-- 117_alarms_interrupt_playback.sql
--
-- renesenses/tune-server-rust#8615 (fil 2784, decision de Bertrand du 13/11) :
-- l'option de reveil « Interrompre la lecture en cours ». 1 : le reveil
-- programme remplace la lecture d'une zone qui joue ; 0 : la zone garde sa
-- musique (regle de #5669). COCHEE par defaut, y compris pour les reveils
-- existants. Jumelle de la migration SQLite 151.
--
-- Meme patron que la 107 (`tracks.explicit`) : sur une base nee d'une bascule,
-- `PG_FULL_SCHEMA` a pose la colonne en TEXT pour la copie ; on la convertit
-- ici en BIGINT, comme l'INTEGER de SQLite. Idempotent.

BEGIN;

ALTER TABLE alarms ADD COLUMN IF NOT EXISTS interrupt_playback BIGINT NOT NULL DEFAULT 1;

DO $reveil_interrompt$
DECLARE
    typ TEXT;
BEGIN
    SELECT data_type INTO typ
      FROM information_schema.columns
     WHERE table_schema = current_schema() AND table_name = 'alarms'
       AND column_name = 'interrupt_playback';
    IF typ IN ('text', 'character varying', 'integer', 'smallint', 'boolean') THEN
        ALTER TABLE alarms ALTER COLUMN interrupt_playback DROP DEFAULT;
        ALTER TABLE alarms ALTER COLUMN interrupt_playback TYPE BIGINT USING (
            CASE WHEN interrupt_playback::text ~ '^[0-9]+$'
                 THEN interrupt_playback::text::bigint ELSE 1 END
        );
        RAISE NOTICE 'migration 117 : alarms.interrupt_playback % -> bigint', typ;
    END IF;
    UPDATE alarms SET interrupt_playback = 1 WHERE interrupt_playback IS NULL;
    ALTER TABLE alarms ALTER COLUMN interrupt_playback SET DEFAULT 1;
    ALTER TABLE alarms ALTER COLUMN interrupt_playback SET NOT NULL;
END $reveil_interrompt$;

INSERT INTO schema_version (version, name) VALUES (117, 'alarms_interrupt_playback')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
