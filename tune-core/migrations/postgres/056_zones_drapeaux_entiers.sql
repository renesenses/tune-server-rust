-- 056_zones_drapeaux_entiers.sql
--
-- QUATRE drapeaux convertis en SMALLINT, chacun apres que son redacteur ait
-- ete repare dans le MEME commit. Pas un de plus : la consigne de #2995 tient,
-- et la regle degagee par #3715 la precise.
--
-- # La regle, et pourquoi elle autorise ces quatre-la
--
-- Mesure du 09/09/2026 (#3715) : une colonne TEXT ne se convertit en type
-- numerique QUE si tous ses redacteurs lient deja un entier. PostgreSQL REFUSE
-- l'affectation `text -> smallint` et `boolean -> smallint` :
--
--   ERROR:  column "dsp_enabled" is of type smallint but expression is of type text
--   ERROR:  column "is_admin" is of type smallint but expression is of type boolean
--
-- C'est ce qui avait ECARTE `zones.dsp_enabled` et `profiles.is_admin` de la
-- 053. Le commit qui porte ce script repare les redacteurs — `ZoneRepo` lie
-- desormais des `i64`, `routes/cloud.rs` aussi — et c'est cette reparation, et
-- elle seule, qui rend la conversion possible. L'ordre exige par #3726 est
-- respecte : d'abord le code, ensuite la colonne, dans le meme commit.
--
-- # Colonne par colonne — la MESURE, pas l'intention
--
-- Relevee le 11/09/2026 sur PostgreSQL 16.15, contre les deux bases que
-- `pg_sqlite_type_parity` monte : `native` (scripts numerotes + ensure_schema)
-- et `migree` (PG_FULL_SCHEMA + scripts + ensure_schema).
--
-- ## 1. zones.is_hidden : text -> smallint  (native ET migree)
--
-- Elle n'est posee par AUCUN script numerote : seul `ENSURE_COLUMNS` la cree,
-- en `TEXT DEFAULT '0'`. Les deux chemins la portent donc en TEXT, et NEUF des
-- onze requetes de `zone_repo.rs` qui la touchent tombent, sur les DEUX
-- chemins :
--
--   ERROR:  COALESCE types text and integer cannot be matched
--   ERROR:  operator does not exist: text = integer
--
-- Ce que cela coute, mesure requete par requete :
--
--   * `list()` — `WHERE COALESCE(is_hidden, 0) = 0` echoue, et son `Err(_)`
--     attrape-tout se rabat sur `list_all()` : une zone SUPPRIMEE reparait
--     dans la liste. Supprimer une zone est sans effet visible sur PostgreSQL.
--   * `count()`, `count_online()`, `count_active()` — les trois comptes de
--     zones, c'est-a-dire le plafond du palier gratuit, echouent et rendent 0.
--   * `is_device_hidden()`, `hidden_by_device`, `hide_by_device`,
--     `unhide_by_device`, `delete_orphans` — tous morts.
--
-- Les deux seules qui passaient sont les affectations pures
-- (`UPDATE zones SET is_hidden = 1 WHERE id = $1`) : `integer -> text` est
-- accepte. C'est precisement ce qui rendait le defaut silencieux — la
-- suppression s'ecrivait, et la lecture ne la voyait jamais.
--
-- Aucun redacteur ne lie de parametre sur cette colonne : les cinq ecritures
-- du depot posent le litteral `0` ou `1` (recherche exhaustive sur
-- tune-core/src et tune-server/src). Rien a reparer dans le code pour
-- celle-ci ; la conversion suffit.
--
-- ## 2. zones.online : text -> smallint  (chemin migre)
--
-- `count_online()` et `count_active()` comparent `online = 1`. Sur une base
-- migree, `online` est TEXT :
--
--   ERROR:  operator does not exist: text = integer
--
-- Et dans l'autre sens, sur une base NATIVE ou `online` est SMALLINT,
-- `ZoneRepo::update_online` et `set_online_by_device` liaient une CHAINE :
--
--   ERROR:  column "online" is of type smallint but expression is of type text
--
-- Aucune zone n'etait donc jamais marquee en ligne sur une installation
-- PostgreSQL native. Les deux redacteurs lient un `i64` depuis ce commit.
--
-- ## 3. zones.dsp_enabled : text -> smallint  (chemin migre)
--
-- `get_dsp_config` lit `COALESCE(dsp_enabled, 0)` : sur une base migree,
-- `COALESCE types text and integer cannot be matched`. Le reglage DSP d'une
-- zone est donc illisible sur tout le parc migre. `ZoneRepo::update_dsp` lie
-- desormais `Option<i64>` et `i64` (il liait `Option<String>` et `String`, ce
-- qui le rendait mort sur TOUTE base PostgreSQL, natif compris — `dsp_preset_id`
-- est BIGINT des deux cotes). #3726, point 1.
--
-- ## 4. profiles.is_admin : text -> smallint  (chemin migre)
--
-- `as_bool()` rend `None` sur un `SqlValue::Text`. Sur une base migree, donc :
--
--   * `POST /auth/login` (auth.rs) lit `is_admin` par `as_bool().unwrap_or(false)`
--     — un administrateur se connecte avec le role `user`, en silence ;
--   * `GET /auth/me` rend `is_admin: null`.
--
-- Et sur une base NATIVE, ou la colonne est SMALLINT (005), la creation de
-- profil SSO de `routes/cloud.rs` liait un BOOLEEN :
--
--   ERROR:  column "is_admin" is of type smallint but expression is of type boolean
--
-- Aucune premiere connexion SSO n'y creait donc de profil. Ce site lie un
-- `i64` depuis ce commit. #3726, point 2.
--
-- # 5. La sequence de `profiles`, remise au niveau
--
-- Ce n'est pas une conversion de type, et c'est ici parce que c'est la TROISIEME
-- couche du meme defaut — « la creation de profil SSO est cassee en natif ».
--
-- La 005 seme le profil par defaut avec un identifiant EXPLICITE :
--
--   INSERT INTO profiles (id, username, display_name, is_admin)
--       VALUES (1, 'default', 'Default', 1) ON CONFLICT (id) DO NOTHING;
--
-- Un INSERT qui pose `id` lui-meme n'avance PAS la sequence. MESURE du
-- 11/09/2026 sur une base NATIVE neuve : `profiles_id_seq` est a 1 avec
-- `is_called = false`, donc le premier `nextval` rend 1 — l'identifiant deja
-- pris :
--
--   ERROR:  duplicate key value violates unique constraint "profiles_pkey"
--   DETAIL:  Key (id)=(1) already exists.
--
-- La toute premiere creation de profil echoue donc sur toute installation
-- PostgreSQL native : premiere connexion SSO (`routes/cloud.rs`) comme
-- inscription locale (`auth.rs`). Les deux avalent l'erreur — `.unwrap_or(0)`
-- et `.ok()` — et le defaut est MUET. L'INSERT rate consomme quand meme la
-- valeur, donc la tentative suivante passe : le symptome est « la premiere fois
-- ca ne marche pas, la seconde si », le pire des symptomes a diagnostiquer.
--
-- Balayage de TOUTES les sequences d'identite des deux bases, le 11/09/2026 :
-- `profiles_id_seq` est la SEULE en retard. C'est aussi le seul `INSERT` des
-- scripts numerotes qui pose un `id` litteral.
--
-- La remise a niveau ne peut faire que MONTER la sequence (`GREATEST`), donc
-- elle est idempotente et ne peut pas fabriquer de collision.
--
-- # Ce qui N'EST PAS converti
--
-- Les cinq lignes restantes de `ECARTS_TOLERES` gardent leur motif mesure :
-- `streaming_favorites.id` et `.profile_id` (la reparation est INVERSE — il
-- faut aligner le natif ET la liaison du depot), `alarms.source_id` (c'est la
-- declaration SQLite qui a tort : la colonne porte une chaine), et les
-- reglages de zone dont aucune requete ne compare la valeur.
--
-- # Surete — le mecanisme est repris mot pour mot de la 053
--
--   * idempotent : on ne touche la colonne que tant qu'elle est text/varchar,
--     donc no-op sur une base native, deja convertie, ou rejouee ;
--   * cast garde : conversion UNIQUEMENT si toute valeur non nulle est un
--     entier litteral, sinon on SAUTE avec un NOTICE plutot que d'avorter ;
--   * normalisation booleenne AVANT le comptage, et ce n'est pas de la
--     precaution : c'est une MESURE. En comptant les collisions sur une base
--     migree le 11/09/2026, `profiles.is_admin` en portait UNE —
--
--       id | username | is_admin
--        1 | default  | 1
--        2 | a@b.c    | true
--
--     La ligne 2 est un profil cree par SSO. C'est la SIGNATURE du defaut du
--     point 4 : `routes/cloud.rs` liait un BOOLEEN, PostgreSQL accepte
--     `boolean -> text`, et il a ecrit le litteral `true` la ou le reste du
--     parc ecrit `1`. Sans cette normalisation, le garde `bad = 0` aurait SAUTE
--     la conversion sur toute base ou un administrateur s'est connecte par SSO
--     — exactement les bases qui en ont le plus besoin — et l'aurait fait en
--     NOTICE, donc en silence. Les orthographes booleennes de PostgreSQL sont
--     donc ramenees a 1/0 avant le comptage, et le nombre de lignes reecrites
--     est annonce ;
--   * le DEFAULT texte est retire avant l'ALTER TYPE puis repose en entier ;
--   * `zones.is_hidden` est d'abord AJOUTEE si elle manque : aucun script
--     numerote ne la declarait, et c'est ce trou qui la laissait en TEXT.
--     `ENSURE_COLUMNS` la declare desormais en SMALLINT elle aussi, sur le
--     modele de `listen_history.album_id` (#2860) ;
--   * aucun declencheur ne porte sur `zones` ni `profiles` (les
--     `*_search_tsv_trg` de la 002 sont sur artists/albums/tracks).
--
-- Le type vise est celui qu'une installation PostgreSQL NATIVE porte DEJA pour
-- `online`, `dsp_enabled` et `is_admin`. `is_hidden` fait exception : elle est
-- TEXT des deux cotes, et SMALLINT devient le type des deux cotes — celui que
-- SQLite declare depuis toujours (`is_hidden INTEGER DEFAULT 0`).
BEGIN;

ALTER TABLE zones ADD COLUMN IF NOT EXISTS is_hidden SMALLINT DEFAULT 0;

DO $migration$
DECLARE
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  -- {table, colonne, type_vise}
  cols     TEXT[][] := ARRAY[
    ['zones','is_hidden','smallint'],
    ['zones','online','smallint'],
    ['zones','dsp_enabled','smallint'],
    ['profiles','is_admin','smallint']
  ];
  c        TEXT[];
  cur_type TEXT;
  col_def  TEXT;
  bad      BIGINT;
  redressees BIGINT;
BEGIN
  FOREACH c SLICE 1 IN ARRAY cols LOOP
    SELECT data_type, column_default INTO cur_type, col_def
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];
    IF cur_type IN ('text', 'character varying') THEN
      -- Les orthographes booleennes que PostgreSQL ecrit quand un redacteur
      -- lie un `boolean` dans une colonne TEXT. Ramenees a 1/0 AVANT le
      -- comptage, sinon le garde ci-dessous saute la conversion en silence.
      EXECUTE format(
        'UPDATE %I SET %I = CASE WHEN lower(%I) IN (''true'',''t'',''yes'',''y'',''on'') '
        'THEN ''1'' ELSE ''0'' END '
        'WHERE lower(%I) IN (''true'',''t'',''yes'',''y'',''on'',''false'',''f'',''no'',''n'',''off'')',
        c[1], c[2], c[2], c[2]);
      GET DIAGNOSTICS redressees = ROW_COUNT;
      IF redressees > 0 THEN
        RAISE NOTICE 'migration 056: %.% — % ligne(s) booleennes ramenees a 1/0', c[1], c[2], redressees;
      END IF;
      EXECUTE format(
        'SELECT count(*) FROM %I WHERE %I IS NOT NULL AND %I !~ %L',
        c[1], c[2], c[2], int_re) INTO bad;
      IF bad = 0 THEN
        IF col_def IS NOT NULL THEN
          EXECUTE format('ALTER TABLE %I ALTER COLUMN %I DROP DEFAULT', c[1], c[2]);
        END IF;
        EXECUTE format(
          'ALTER TABLE %I ALTER COLUMN %I TYPE %s USING %I::%s',
          c[1], c[2], c[3], c[2], c[3]);
        IF col_def ~ '^''?-?[0-9]+''?(::text)?$' THEN
          EXECUTE format('ALTER TABLE %I ALTER COLUMN %I SET DEFAULT %s',
            c[1], c[2], regexp_replace(col_def, '[^0-9-]', '', 'g'));
        END IF;
        RAISE NOTICE 'migration 056: %.% text->%', c[1], c[2], c[3];
      ELSE
        RAISE NOTICE 'migration 056: SKIP %.% (% valeurs non entieres)', c[1], c[2], bad;
      END IF;
    END IF;
  END LOOP;
END
$migration$;

-- La sequence de `profiles`, remise au niveau du plus grand identifiant pose.
-- `GREATEST` : elle ne peut que monter, jamais descendre.
-- La sequence de `profiles`, remise au niveau du plus grand identifiant pose.
-- `GREATEST` : elle ne peut que monter, jamais descendre.
SELECT setval(
  pg_get_serial_sequence('profiles', 'id'),
  GREATEST((SELECT COALESCE(max(id), 1) FROM profiles), 1),
  true
)
WHERE pg_get_serial_sequence('profiles', 'id') IS NOT NULL;

INSERT INTO schema_version (version, name) VALUES (56, 'zones_drapeaux_entiers')
  ON CONFLICT (version) DO NOTHING;
COMMIT;
