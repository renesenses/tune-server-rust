-- #8838 : vide conserve le fuseau système et les heures existantes.
INSERT INTO settings (key, value) VALUES ('alarms_timezone', '')
ON CONFLICT (key) DO NOTHING;
