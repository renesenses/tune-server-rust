-- 047_zones_volume_a_virgule.sql
--
-- #2886 — `zones.volume` etait un INTEGER borne 0..100. Le volume commande
-- (0..1 en virgule flottante) etait donc arrondi avant persistance :
--
--   update_volume(id, (v.clamp(0,1) * 100.0).round() as i32)
--
-- Deux consequences, mesurees :
--   * vers -37 dB, l'arrondi coute deja 3 dB — on repose le volume, il a bouge ;
--   * sous 0,005 lineaire, soit **-46,0205999133 dB exactement**
--     (20*log10(0,005)), l'entier tombe a 0 : apres un redemarrage la zone se
--     rallume MUETTE, alors que l'utilisateur l'avait laissee tres basse mais
--     audible. Le seuil n'est pas « environ -46 dB » : c'est le point ou
--     `round(v * 100)` bascule de 1 a 0.
--
-- Le correctif elargit le TYPE de la colonne, **sans toucher a l'echelle**.
-- Elle reste en pour-cent 0..100 : ainsi 50 reste 50, aucune ligne existante
-- n'a besoin d'etre convertie, et une migration a moitie appliquee ne peut pas
-- diviser (ou multiplier) le volume de qui que ce soit par 100.
--
-- Trois etats de depart possibles pour cette colonne, tous couverts :
--   * `integer`          — base PG creee par 001 ;
--   * `text`/`varchar`   — base issue de `tune db migrate-to-postgres`
--                          (PG_FULL_SCHEMA declare tout en TEXT) dont la
--                          migration 013 n'a pas encore tourne ;
--   * `double precision` — deja soignee : la migration est un no-op strict.
--
-- Idempotente : re-jouable sans effet sur une base deja convertie.

BEGIN;

DO $migration_2886$
DECLARE
  cur_type TEXT;
BEGIN
  SELECT data_type INTO cur_type
    FROM information_schema.columns
   WHERE table_name = 'zones' AND column_name = 'volume';

  IF cur_type IS NULL THEN
    RAISE NOTICE 'zones.volume absente — rien a convertir';
  ELSIF cur_type = 'double precision' THEN
    RAISE NOTICE 'zones.volume deja en double precision — no-op';
  ELSE
    -- Le DEFAULT doit tomber d'abord : PG refuse de recaster un defaut dont
    -- le type ne suit pas automatiquement.
    ALTER TABLE zones ALTER COLUMN volume DROP DEFAULT;

    -- Une valeur qui n'est pas un nombre (colonne TEXT d'une base migree)
    -- devient NULL plutot que d'avorter toute la migration : la lecture
    -- retombe alors sur son defaut applicatif, comme pour toute zone neuve.
    ALTER TABLE zones
      ALTER COLUMN volume TYPE DOUBLE PRECISION
      USING CASE
              WHEN volume::text ~ '^-?[0-9]+(\.[0-9]+)?$'
                THEN volume::text::double precision
              ELSE NULL::double precision
            END;

    ALTER TABLE zones ALTER COLUMN volume SET DEFAULT 50;
  END IF;
END
$migration_2886$;

INSERT INTO schema_version (version, name)
VALUES (47, 'zones_volume_a_virgule')
ON CONFLICT (version) DO NOTHING;

COMMIT;
