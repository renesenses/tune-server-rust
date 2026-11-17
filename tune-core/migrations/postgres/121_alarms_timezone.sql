BEGIN;

-- #8838 : vide conserve le fuseau système et les heures existantes.
INSERT INTO settings (key, value) VALUES ('alarms_timezone', '')
ON CONFLICT (key) DO NOTHING;

INSERT INTO schema_version (version, name) VALUES (121, 'alarms_timezone')
ON CONFLICT (version) DO NOTHING;

COMMIT;
