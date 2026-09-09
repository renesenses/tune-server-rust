-- 053_parite_types_pg_sqlite.sql
--
-- TROIS colonnes de la liste de #3715, converties une par une. Pas les treize :
-- #2995 l'interdit, et la mesure lui donne raison — sur les treize, TROIS
-- seulement peuvent etre converties sans casser une ecriture existante.
--
-- # Ce qui a decide, colonne par colonne
--
-- La regle degagee par la mesure du 09/09/2026 sur PostgreSQL 16.15 : une
-- colonne TEXT ne se convertit en type numerique QUE si tous ses redacteurs
-- lient deja un entier. PostgreSQL REFUSE l'affectation `text -> smallint` et
-- `boolean -> smallint` :
--
--   ERROR:  column "dsp_enabled" is of type smallint but expression is of type text
--   ERROR:  column "is_admin" is of type smallint but expression is of type boolean
--
-- alors qu'il accepte `bigint -> text`. Convertir une colonne dont un
-- redacteur lie du texte la ferait passer de « lecture fausse » a « ecriture
-- refusee » : on echangerait un silence contre une panne. C'est ce qui ecarte
-- `zones.dsp_enabled` et `profiles.is_admin` de ce script, malgre le point 2 de
-- #3715 — leur redacteur doit etre repare AVANT, dans le code.
--
-- ## 1. album_metadata.album_id : text -> bigint
--
-- Ce qu'elle casse, MESURE sur une base migree :
--
--   ERROR:  operator does not exist: text = bigint
--   LINE 1: SELECT key, value FROM album_metadata WHERE album_id = $1 ORDER BY key
--
-- `AlbumMetadataRepo::get_all/delete_one/delete_all` lient `album_id` en `i64`
-- (album_metadata_repo.rs). Sur toute base PostgreSQL issue d'une bascule
-- SQLite, les metadonnees d'album etendues — chef d'orchestre, interprete,
-- code-barres, numero de catalogue — sont donc ILLISIBLES. Meme forme exacte
-- que #2860, et meme silence : l'erreur remonte a un appelant qui la traduit en
-- « aucune metadonnee ».
--
-- Aucune regression possible : les DEUX redacteurs (`set`, `set_batch`) lient
-- `&album_id` en `i64`, et une base NATIVE porte deja `bigint` — l'INSERT y
-- tourne tel quel depuis toujours.
--
-- ## 2. network_mounts.active : text -> smallint
--
-- Ce qu'elle casse, MESURE sur une base migree :
--
--   ERROR:  COALESCE types text and integer cannot be matched
--   LINE 1: ... FROM network_mounts WHERE mount_type = 'smb' AND COALESCE(active, 1) = 1
--
-- C'est la requete de `tune-server/src/startup.rs` qui remonte les partages SMB
-- au demarrage. Sur une base migree elle echoue : AUCUN partage reseau n'est
-- remonte au boot, et la bibliotheque parait vide. `active` porte l'INTENTION
-- de l'utilisateur — exactement le mecanisme que #1916 rendait invisible.
--
-- Aucune regression possible : `active` n'a AUCUN redacteur dans le depot
-- (recherche exhaustive sur tune-core/src et tune-server/src). Elle n'est
-- qu'ecrite par son DEFAULT et lue par ce COALESCE.
--
-- ## 3. listen_history.context_position : text -> bigint
--
-- La seule des trois lignes `[native]` de #3715 qui se repare sans toucher au
-- code. Rien ne casse aujourd'hui — l'INSERT lie un `Option<i64>` (assignation
-- `bigint -> text` acceptee) et la lecture passe par `as_i64()` qui reparse le
-- texte. Elle est convertie quand meme, et c'est deliberer :
--
--   * c'est un RANG, la migration 046 l'a declaree TEXT par inadvertance il y a
--     quatre jours — il n'existe donc aucune valeur ancienne a menager ;
--   * son unique redacteur (`HistoryRepo::record`) lie deja `Option<i64>` ;
--   * son unique lecteur (`as_i64()`) accepte les deux types ;
--   * elle diverge sur les DEUX chemins : cette ligne en repare deux.
--
-- Laisser un rang en TEXT, c'est garder arme le `text = bigint` de #2860 pour
-- la premiere requete qui voudra le comparer ou le trier.
--
-- # Ce qui N'EST PAS converti, et pourquoi
--
-- Les neuf lignes restantes sont inscrites nominativement dans
-- `ECARTS_TOLERES` (tune-core/src/db/pg_sqlite_type_parity.rs) avec leur motif
-- MESURE. Le detail est dans #3715.
--
-- # Surete — le mecanisme est repris mot pour mot de la 049 et de la 012
--
--   * idempotent : on ne touche la colonne que tant qu'elle est text/varchar,
--     donc no-op sur une base native, deja convertie, ou rejouee ;
--   * cast garde : conversion UNIQUEMENT si toute valeur non nulle est un
--     entier litteral, sinon on SAUTE avec un NOTICE plutot que d'avorter ;
--   * le DEFAULT texte est retire avant l'ALTER TYPE puis repose en entier —
--     sans quoi `network_mounts.active DEFAULT 1` deviendrait ininserrable ;
--   * aucun declencheur ne porte sur ces trois tables (les `*_search_tsv_trg`
--     de la 002 sont sur artists/albums/tracks), donc pas de DROP/CREATE
--     TRIGGER a orchestrer comme le fait la 012.
--
-- Le type vise est, pour chacune, celui qu'une installation PostgreSQL NATIVE
-- porte DEJA : bigint pour `album_metadata.album_id` (005), smallint pour
-- `network_mounts.active` (008). Ce script n'invente aucun etat — il amene la
-- base migree la ou tout parc natif tourne depuis toujours.
-- `listen_history.context_position` fait exception : elle est TEXT des deux
-- cotes, et c'est bigint qui devient le type des deux cotes.

BEGIN;

DO $migration$
DECLARE
  int_re   CONSTANT TEXT := '^-?[0-9]+$';
  -- {table, colonne, type_vise}
  cols     TEXT[][] := ARRAY[
    ['album_metadata','album_id','bigint'],
    ['network_mounts','active','smallint'],
    ['listen_history','context_position','bigint']
  ];
  c        TEXT[];
  cur_type TEXT;
  col_def  TEXT;
  bad      BIGINT;
BEGIN
  FOREACH c SLICE 1 IN ARRAY cols LOOP
    SELECT data_type, column_default INTO cur_type, col_def
      FROM information_schema.columns
     WHERE table_name = c[1] AND column_name = c[2];

    IF cur_type IN ('text', 'character varying') THEN
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

        RAISE NOTICE 'migration 053: %.% text->%', c[1], c[2], c[3];
      ELSE
        RAISE NOTICE 'migration 053: SKIP %.% (% valeurs non entieres)', c[1], c[2], bad;
      END IF;
    END IF;
  END LOOP;
END
$migration$;

INSERT INTO schema_version (version, name) VALUES (53, 'parite_types_pg_sqlite')
ON CONFLICT (version) DO NOTHING;

COMMIT;
