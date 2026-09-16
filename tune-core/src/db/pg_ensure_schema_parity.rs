//! Le TROISIÈME axe : `ensure_schema` confronté aux scripts numérotés.
//!
//! `queue_items` a trois rédacteurs de schéma en PostgreSQL — les scripts
//! numérotés, `PG_FULL_SCHEMA`, et le DDL auto-réparateur `ensure_schema` — et
//! il n'existait de porte que sur deux des trois paires :
//!
//! | porte | compare | ticket |
//! |---|---|---|
//! | `pg_schema_parity` | `PG_FULL_SCHEMA` ↔ scripts numérotés | #2111 |
//! | `pg_sqlite_type_parity` | PostgreSQL ↔ SQLite | #2995 |
//! | **celui-ci** | **`ensure_schema` ↔ scripts numérotés** | **#3716** |
//!
//! Aucune des deux premières ne regarde `ensure_schema` — et c'est pourtant lui
//! qui s'exécute **à chaque démarrage**, AVANT le lanceur de migrations
//! (`connect()` appelle `ensure_schema()` puis `run_pg_migrations()`).
//!
//! ## Pourquoi le premier démarrage donne un autre schéma
//!
//! Sur une base VIDE, les `CREATE TABLE IF NOT EXISTS` d'`ENSURE_TABLES`
//! réussissent : ils créent réellement les tables. Les scripts numérotés
//! passent ensuite, et leurs propres `CREATE TABLE IF NOT EXISTS` sont alors
//! des no-op — la table existe déjà, avec les types d'`ensure_schema`.
//!
//! Un `CREATE TABLE IF NOT EXISTS` n'altère jamais une table existante : c'est
//! donc l'installation NEUVE qui hérite des types d'`ensure_schema`, et les
//! scripts numérotés ne la rattrapent que s'ils portent un `ALTER … USING`.
//!
//! C'est exactement le mécanisme de #1706 : `ensure_schema` réimposait un
//! `nextval(…)::text` sur une colonne BIGINT, la transaction entière sautait,
//! et neuf zones du `.15` perdaient leur file à chaque démarrage. Là, la
//! divergence coûtait cher.
//!
//! ## Ce que la porte monte, et pourquoi deux bases
//!
//! | base | ce qui la monte | ce qu'elle représente |
//! |---|---|---|
//! | `scripts` | `PG_MIGRATIONS` seuls | ce que les scripts numérotés DÉCLARENT |
//! | `demarrage` | `ensure_schema`, puis `PG_MIGRATIONS`, puis `ensure_schema` | ce qu'une installation NEUVE reçoit vraiment |
//!
//! La seconde reproduit `connect()` au mot près, y compris le second passage
//! d'`ensure_schema` — celui qui rejoue à chaque démarrage suivant.
//!
//! On compare le `data_type` **exact**, pas la famille : tout le sujet de #3716
//! est `bigint` là où la 013 déclare `integer`. Les deux sont des entiers ; ce
//! n'est pas la valeur qui est en cause, c'est que **deux rédacteurs du même
//! schéma ne s'accordent pas et que rien ne les compare**.
//!
//! ## Pourquoi de vraies bases, et pas un parseur
//!
//! Même raison que pour les deux frères : un parseur qui ne reconnaîtrait plus
//! une forme SQL rendrait moins de colonnes, donc moins d'écarts, donc un test
//! **vert par ignorance**. Ici c'est PostgreSQL lui-même qui répond sur ce
//! qu'il vient de créer.
#![cfg(all(test, feature = "postgres"))]

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Connection, PgConnection, Row};

/// Les deux bases jetables, recréées à chaque exécution.
const BASE_SCRIPTS: &str = "tune_ecrivains_scripts";
const BASE_DEMARRAGE: &str = "tune_ecrivains_demarrage";

/// Les divergences connues au **09/09/2026**, datées et motivées.
///
/// `(table, colonne, type_scripts, type_demarrage, motif)`.
///
/// ⚠️ Ces lignes ne disent pas « c'est bien ». Elles disent « c'est connu, daté,
/// et ça se traite une par une ». Le jour où une colonne est alignée, sa ligne
/// doit disparaître d'ici — `aucune_divergence_perimee` le vérifie.
// #3716: migration 059 aligns all three queue columns to the startup BIGINT
// representation. No exception remains; new drift still fails the real DB gate.
const DIVERGENCES_TOLEREES: &[(&str, &str, &str, &str, &str)] = &[];

fn url_vers_base(url: &str, base: &str) -> String {
    let (avant, apres) = match url.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (url, None),
    };
    let racine = avant.rsplit_once('/').map(|(r, _)| r).unwrap_or(avant);
    match apres {
        Some(q) => format!("{racine}/{base}?{q}"),
        None => format!("{racine}/{base}"),
    }
}

async fn connexion(url: &str) -> PgConnection {
    PgConnection::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connexion à {url} impossible : {e}"))
}

async fn base_vierge(maintenance: &mut PgConnection, nom: &str, url_racine: &str) -> PgConnection {
    // `AssertSqlSafe` : `nom` est l'une des deux constantes de ce fichier, et
    // PostgreSQL n'accepte de toute façon pas de paramètre lié en DDL.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {nom}"
    )))
    .execute(&mut *maintenance)
    .await
    .unwrap_or_else(|e| panic!("suppression de {nom} : {e}"));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {nom}")))
        .execute(&mut *maintenance)
        .await
        .unwrap_or_else(|e| panic!("création de {nom} : {e}"));

    let mut c = connexion(&url_vers_base(url_racine, nom)).await;
    sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS unaccent")
        .execute(&mut c)
        .await
        .unwrap_or_else(|e| panic!("extension unaccent sur {nom} : {e}"));
    c
}

/// Les scripts numérotés, dans l'ordre — et rien d'autre.
async fn scripts_numerotes(c: &mut PgConnection) {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TIMESTAMPTZ DEFAULT now(),
            name TEXT NOT NULL
        )",
    )
    .execute(&mut *c)
    .await
    .expect("schema_version");

    for (version, nom, sql) in crate::db::migrations::PG_MIGRATIONS {
        sqlx::raw_sql(*sql)
            .execute(&mut *c)
            .await
            .unwrap_or_else(|e| panic!("migration {version:03}_{nom} : {e}"));
    }
}

/// Le DDL auto-réparateur, joué comme `run_each` le joue : une instruction par
/// aller-retour, et un échec ne bloque jamais la suivante.
///
/// C'est cette règle qui a été payée cher (#1706) : réunies en un seul
/// `raw_sql`, ces instructions passaient par le protocole simple, donc dans UNE
/// transaction implicite, et une seule qui tombait annulait tout le lot.
async fn ensure_schema(c: &mut PgConnection) {
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&mut *c).await;
    }
}

/// `data_type` de chaque colonne du schéma `public`, groupé par table.
async fn types_pg(c: &mut PgConnection) -> BTreeMap<String, BTreeMap<String, String>> {
    let lignes = sqlx::query(
        "SELECT table_name, column_name, data_type \
         FROM information_schema.columns \
         WHERE table_schema = 'public'",
    )
    .fetch_all(c)
    .await
    .expect("lecture d'information_schema");

    let mut par_table: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for l in lignes {
        let table: String = l.get("table_name");
        let colonne: String = l.get("column_name");
        let t: String = l.get("data_type");
        par_table.entry(table).or_default().insert(colonne, t);
    }
    par_table
}

/// Monte les deux bases et rend `(scripts, demarrage)`.
///
/// Les deux témoins de ce fichier en ont besoin ; ils ne peuvent pas partager
/// leurs bases (ils tournent en parallèle dans le même binaire), donc le suffixe
/// distingue leurs jeux.
async fn monter_les_deux(
    url: &str,
    suffixe: &str,
) -> (
    BTreeMap<String, BTreeMap<String, String>>,
    BTreeMap<String, BTreeMap<String, String>>,
) {
    let mut maintenance = connexion(url).await;

    let mut scripts = base_vierge(&mut maintenance, &format!("{BASE_SCRIPTS}{suffixe}"), url).await;
    scripts_numerotes(&mut scripts).await;

    // L'ordre EXACT de `connect()` sur une base vide, puis le rejeu du
    // démarrage suivant.
    let mut demarrage =
        base_vierge(&mut maintenance, &format!("{BASE_DEMARRAGE}{suffixe}"), url).await;
    ensure_schema(&mut demarrage).await;
    scripts_numerotes(&mut demarrage).await;
    ensure_schema(&mut demarrage).await;

    let t_scripts = types_pg(&mut scripts).await;
    let t_demarrage = types_pg(&mut demarrage).await;

    // Garde-fou du garde-fou : si l'une des lectures rendait peu de choses —
    // mauvaise base, schéma vide, requête muette — la comparaison serait vide
    // et le test vert sans avoir rien vérifié.
    for (nom, m) in [
        ("scripts numérotés", &t_scripts),
        ("premier démarrage", &t_demarrage),
    ] {
        assert!(
            m.get("tracks").is_some_and(|c| c.len() > 20),
            "{nom} n'a pas monté `tracks` — le test ne vérifiait rien"
        );
        assert!(
            m.contains_key("queue_items"),
            "{nom} n'a pas monté `queue_items` — le test ne vérifiait rien"
        );
    }

    (t_scripts, t_demarrage)
}

/// Les écarts de `data_type` entre les deux rédacteurs.
///
/// Ne compare que les tables ET colonnes présentes des deux côtés : quatre
/// tables n'existent QUE dans `ensure_schema` (`file_first_seen`, `task_runs`,
/// `streaming_favorites`, `streaming_item_tags`) — leur absence des scripts est
/// le sujet de `pg_schema_parity`, pas celui-ci.
fn ecarts(
    scripts: &BTreeMap<String, BTreeMap<String, String>>,
    demarrage: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<(String, String, String, String)> {
    let mut v = Vec::new();
    for (table, colonnes_scripts) in scripts {
        let Some(colonnes_dem) = demarrage.get(table) else {
            continue;
        };
        for (colonne, type_scripts) in colonnes_scripts {
            let Some(type_dem) = colonnes_dem.get(colonne) else {
                continue;
            };
            if type_scripts != type_dem {
                v.push((
                    table.clone(),
                    colonne.clone(),
                    type_scripts.clone(),
                    type_dem.clone(),
                ));
            }
        }
    }
    v
}

/// Deux rédacteurs du même schéma qui ne s'accordent pas, et personne pour
/// les confronter : c'est le trou que #3716 nomme.
#[tokio::test]
async fn parite_ensure_schema_scripts_numerotes() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absent — parite_ensure_schema_scripts_numerotes sautée");
        return;
    };

    let (scripts, demarrage) = monter_les_deux(&url, "").await;

    let toleres: BTreeSet<(&str, &str)> = DIVERGENCES_TOLEREES
        .iter()
        .map(|(t, c, _, _, _)| (*t, *c))
        .collect();

    let restants: Vec<String> = ecarts(&scripts, &demarrage)
        .into_iter()
        .filter(|(t, c, _, _)| !toleres.contains(&(t.as_str(), c.as_str())))
        .map(|(t, c, ts, td)| {
            format!("  {t}.{c} : scripts numérotés `{ts}` vs ensure_schema `{td}`")
        })
        .collect();

    assert!(
        restants.is_empty(),
        "Colonnes que les DEUX rédacteurs du schéma PostgreSQL déclarent \
         DIFFÉREMMENT :\n{}\n\n\
         `ensure_schema` (db/postgres.rs — `ENSURE_TABLES` / `ENSURE_COLUMNS`) \
         s'exécute à CHAQUE démarrage, AVANT le lanceur de migrations. Sur une \
         base vide ses `CREATE TABLE IF NOT EXISTS` réussissent : ce sont SES \
         types que l'installation neuve reçoit, et les scripts numérotés ne les \
         rattrapent que s'ils portent un `ALTER … USING`.\n\n\
         Une colonne se type aux QUATRE endroits : `CORE_SCHEMA` (db/sqlite.rs), \
         migration SQLite (db/migrations.rs), `PG_FULL_SCHEMA` (db/pg_migrate.rs) \
         et migration PG (migrations/postgres/NNN_….sql) — plus `ensure_schema`, \
         qui les précède tous.\n\
         Décider quel rédacteur fait foi, puis aligner le perdant. Si l'écart \
         est délibéré et doit attendre, l'inscrire dans `DIVERGENCES_TOLEREES` \
         avec sa date et son motif — jamais en silence. #3716",
        restants.join("\n")
    );
}

/// Une exception qui ne correspond plus à rien doit disparaître.
///
/// C'est la façon habituelle dont un garde-fou meurt : la liste d'exceptions
/// grossit, personne ne l'élague, et elle finit par couvrir tout ce qu'elle
/// devait surveiller.
#[tokio::test]
async fn aucune_divergence_perimee() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absent — aucune_divergence_perimee sautée");
        return;
    };

    let (scripts, demarrage) = monter_les_deux(&url, "_bis").await;

    let vivants: BTreeSet<(String, String)> = ecarts(&scripts, &demarrage)
        .into_iter()
        .map(|(t, c, _, _)| (t, c))
        .collect();

    let mut perimees: Vec<String> = Vec::new();
    for (table, colonne, ts, td, motif) in DIVERGENCES_TOLEREES {
        if !vivants.contains(&(table.to_string(), colonne.to_string())) {
            perimees.push(format!(
                "  {table}.{colonne} — annoncée `{ts}` vs `{td}` — {motif}"
            ));
        }
    }

    assert!(
        perimees.is_empty(),
        "Exceptions périmées dans `DIVERGENCES_TOLEREES` — les deux rédacteurs \
         s'accordent désormais (ou la colonne n'existe plus des deux côtés). \
         Les retirer :\n{}",
        perimees.join("\n")
    );
}

/// L'inventaire doit rester lisible : ni doublon, ni motif vide.
///
/// Ne demande aucune base : c'est la seule épreuve de ce fichier qui tourne
/// dans un `cargo test` ordinaire.
#[test]
fn l_inventaire_des_divergences_est_propre() {
    let mut vus: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (table, colonne, ts, td, motif) in DIVERGENCES_TOLEREES {
        assert!(
            !motif.trim().is_empty(),
            "{table}.{colonne} n'a pas de motif"
        );
        assert_ne!(
            ts, td,
            "{table}.{colonne} : les deux types annoncés sont identiques — \
             ce n'est pas une divergence"
        );
        assert!(
            vus.insert((table, colonne)),
            "doublon dans DIVERGENCES_TOLEREES : {table}.{colonne}"
        );
    }
}

#[test]
fn le_nom_de_base_est_remplace_dans_l_url() {
    assert_eq!(
        url_vers_base("postgresql://tune:tune@localhost:5432/tune_test", "essai"),
        "postgresql://tune:tune@localhost:5432/essai"
    );
    assert_eq!(
        url_vers_base(
            "postgresql://tune:tune@localhost:5432/tune_test?sslmode=disable",
            "essai"
        ),
        "postgresql://tune:tune@localhost:5432/essai?sslmode=disable"
    );
}

/// Exercise the registered migration on existing rows, not just empty DDL.
#[tokio::test]
async fn pg_3716_queue_migration_preserves_rows_defaults_and_replay() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        return;
    };
    let migration = crate::db::migrations::PG_MIGRATIONS
        .iter()
        .find(|(v, _, _)| *v == 59)
        .expect("queue migration must be registered")
        .2;
    let mut maintenance = connexion(&url).await;
    let mut c = base_vierge(&mut maintenance, "tune_queue_types_3716", &url).await;
    sqlx::raw_sql("CREATE TABLE schema_version (version integer PRIMARY KEY, name text)")
        .execute(&mut c)
        .await
        .unwrap();
    for (position, current, duration, large) in [
        ("integer", "smallint", "integer", false),
        ("bigint", "bigint", "bigint", true),
        ("text", "text", "text", true),
    ] {
        sqlx::raw_sql("DROP TABLE IF EXISTS queue_items")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE queue_items (id bigint PRIMARY KEY, position {position} NOT NULL DEFAULT 0,              is_current {current} DEFAULT 0, duration_ms {duration} DEFAULT 0,              track_number bigint, disc_number bigint, title text);              INSERT INTO queue_items VALUES (1, '7', '1', '{}', 12, 2, 'kept'),              (2, '8', NULL, NULL, NULL, NULL, 'nullable')",
            if large { "9223372036854775807" } else { "2147483647" }
        ))).execute(&mut c).await.unwrap();
        for _ in 0..2 {
            sqlx::raw_sql(migration).execute(&mut c).await.unwrap();
            let types = types_pg(&mut c).await;
            for col in [
                "position",
                "is_current",
                "duration_ms",
                "track_number",
                "disc_number",
            ] {
                assert_eq!(
                    types["queue_items"][col], "bigint",
                    "queue_items.{col}: the registered migration must align existing {position}/{current}/{duration} columns"
                );
            }
            let row = sqlx::query("SELECT position, is_current, duration_ms, track_number, disc_number, title FROM queue_items WHERE id=1")
                .fetch_one(&mut c).await.unwrap();
            assert_eq!(row.get::<i64, _>(0), 7);
            assert_eq!(row.get::<i64, _>(1), 1);
            assert_eq!(
                row.get::<i64, _>(2),
                if large { i64::MAX } else { i32::MAX as i64 }
            );
            assert_eq!(row.get::<i64, _>(3), 12);
            assert_eq!(row.get::<i64, _>(4), 2);
            assert_eq!(row.get::<String, _>(5), "kept");
            let nulls = sqlx::query("SELECT is_current, duration_ms FROM queue_items WHERE id=2")
                .fetch_one(&mut c)
                .await
                .unwrap();
            assert_eq!(nulls.get::<Option<i64>, _>(0), None);
            assert_eq!(nulls.get::<Option<i64>, _>(1), None);
        }
        sqlx::query("INSERT INTO queue_items (id) VALUES (3)")
            .execute(&mut c)
            .await
            .unwrap();
        let row =
            sqlx::query("SELECT position, is_current, duration_ms FROM queue_items WHERE id=3")
                .fetch_one(&mut c)
                .await
                .unwrap();
        for i in 0..3 {
            assert_eq!(row.get::<i64, _>(i), 0, "numeric default must survive");
        }
        // Same integer bindings/literals as PlayQueueRepo: both queue writers
        // and current-item queries must remain valid after a legacy TEXT cast.
        sqlx::query("UPDATE queue_items SET position=$1, is_current=$2 WHERE id=3")
            .bind(9i64)
            .bind(1i64)
            .execute(&mut c)
            .await
            .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM queue_items WHERE is_current='1'")
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert_eq!(count, 2);
    }
}

#[tokio::test]
async fn pg_3716_invalid_legacy_values_are_preserved() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        return;
    };
    let migration = crate::db::migrations::PG_MIGRATIONS
        .iter()
        .find(|(v, _, _)| *v == 59)
        .unwrap()
        .2;
    let mut maintenance = connexion(&url).await;
    let mut c = base_vierge(&mut maintenance, "tune_queue_invalid_3716", &url).await;
    sqlx::raw_sql("CREATE TABLE schema_version (version integer PRIMARY KEY, name text);         CREATE TABLE queue_items (position text DEFAULT '0', is_current smallint DEFAULT 0, duration_ms text);         INSERT INTO queue_items VALUES ('not-a-number', 1, '9223372036854775808')")
        .execute(&mut c).await.unwrap();
    sqlx::raw_sql(migration).execute(&mut c).await.unwrap();
    let types = types_pg(&mut c).await;
    assert_eq!(types["queue_items"]["position"], "text");
    assert_eq!(types["queue_items"]["duration_ms"], "text");
    assert_eq!(types["queue_items"]["is_current"], "bigint");
    let row = sqlx::query("SELECT position,duration_ms FROM queue_items")
        .fetch_one(&mut c)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>(0), "not-a-number");
    assert_eq!(row.get::<String, _>(1), "9223372036854775808");
}
