//! Les gardes de migration PostgreSQL lisent le schéma COURANT (#5003).
//!
//! `information_schema.columns` expose les colonnes de TOUS les schémas
//! lisibles de la base, pas seulement celles que les instructions non
//! qualifiées (`ALTER TABLE tracks …`) désignent. Une garde qui lit
//! `WHERE table_name = 'x' AND column_name = 'y'` sans `table_schema` voit
//! donc aussi la table homonyme d'un schéma voisin — copie de sauvegarde,
//! restauration `pg_dump` dans un second schéma, deuxième instance Tune dans
//! la même base :
//!
//! - garde `IF EXISTS (… data_type IN ('text', …))` : le voisin encore en TEXT
//!   la fait tirer sur une colonne déjà BIGINT, et la conversion échoue
//!   (`btrim(bigint) does not exist` — la 038, intégration v0.9.164) ;
//! - garde `SELECT data_type INTO cur_type` : elle lit la ligne du voisin, et
//!   la conversion de la vraie table est sautée en silence (`text = bigint`
//!   au runtime) ou tentée à tort.
//!
//! ## Ce que le test monte
//!
//! Une base jetable par épreuve, et une base TÉMOIN montée par le même code
//! sans voisin. Le schéma de Tune s'appelle `cible` des deux côtés (même nom,
//! donc mêmes défauts affichés) ; le voisin `leurre` est créé AVANT `cible`,
//! pour que ses lignes sortent les premières du catalogue. On compare, table
//! par table, les colonnes (type, nullabilité, défaut) et les déclencheurs de
//! `cible`, plus les instructions d'`ensure_schema` qui ont échoué : un voisin
//! ne doit rien y changer.
//!
//! | épreuve | voisin `leurre` | `cible` |
//! |---|---|---|
//! | natif | `PG_FULL_SCHEMA` seul, tout-TEXT | premier démarrage (ensure, scripts, ensure) |
//! | bascule | premier démarrage natif (BIGINT) | `PG_FULL_SCHEMA` + sentinelle 99, rejeu |
//!
//! ## Nettoyage
//!
//! Les bases sont supprimées à la fin, y compris quand l'épreuve échoue (le
//! corps tourne dans une tâche dont on récupère la panique). Un schéma laissé
//! derrière dans la base partagée de la CI a déjà cassé la garde de la 038 ;
//! c'est pour ça que ce test travaille dans SES bases, jamais dans `tune_test`.
#![cfg(all(test, feature = "postgres"))]

use std::collections::BTreeMap;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool, Row};

const VOISIN: &str = "leurre";
const SCHEMA: &str = "cible";

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

async fn supprimer_base(url: &str, nom: &str) {
    let mut maintenance = PgConnection::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connexion de maintenance : {e}"));
    // `AssertSqlSafe` : `nom` est formé des constantes de ce fichier.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {nom} WITH (FORCE)"
    )))
    .execute(&mut maintenance)
    .await
    .unwrap_or_else(|e| panic!("suppression de {nom} : {e}"));
    let _ = maintenance.close().await;
}

/// Une base neuve, `unaccent` dans `public`, et les schémas demandés créés
/// DANS CET ORDRE (le premier a l'oid le plus bas).
async fn base_vierge(url: &str, nom: &str, schemas: &[&str]) {
    supprimer_base(url, nom).await;
    let mut maintenance = PgConnection::connect(url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {nom}")))
        .execute(&mut maintenance)
        .await
        .unwrap_or_else(|e| panic!("création de {nom} : {e}"));
    let _ = maintenance.close().await;

    let mut c = PgConnection::connect(&url_vers_base(url, nom))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS unaccent")
        .execute(&mut c)
        .await
        .unwrap_or_else(|e| panic!("extension unaccent sur {nom} : {e}"));
    for s in schemas {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {s}")))
            .execute(&mut c)
            .await
            .unwrap_or_else(|e| panic!("schéma {s} sur {nom} : {e}"));
    }
    let _ = c.close().await;
}

/// Un pool dont le schéma courant est `schema` (`public` suit, pour
/// `unaccent`, et ne porte aucune table).
async fn pool_sur(url: &str, base: &str, schema: &'static str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |c, _| {
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "SET search_path TO {schema}, public"
                )))
                .execute(c)
                .await
                .map(|_| ())
            })
        })
        .connect(&url_vers_base(url, base))
        .await
        .unwrap_or_else(|e| panic!("pool {base}/{schema} : {e}"))
}

/// `ensure_schema` joué comme `PostgresDb::run_each` : une instruction par
/// aller-retour, un échec ne bloque pas la suivante. Rend les instructions
/// qui ont échoué.
async fn ensure_schema(pool: &PgPool) -> Vec<String> {
    let mut echecs = Vec::new();
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        if let Err(e) = sqlx::raw_sql(*sql).execute(pool).await {
            echecs.push(format!("{sql} -> {e}"));
        }
    }
    echecs
}

/// L'ordre de `connect()` puis du lanceur au premier démarrage, puis le rejeu
/// d'`ensure_schema` du démarrage suivant.
async fn demarrage_natif(pool: &PgPool) -> (Result<(), String>, Vec<String>) {
    let mut echecs = ensure_schema(pool).await;
    let r = migrer(pool).await;
    echecs.extend(ensure_schema(pool).await);
    (r, echecs)
}

/// `run_pg_migrations`, puis un `ROLLBACK` quand un script a échoué : le
/// script meurt entre son `BEGIN` et son `COMMIT`, et la connexion unique du
/// pool resterait dans une transaction avortée (25P02) — chaque requête
/// suivante échouerait sans rien dire de la cause.
async fn migrer(pool: &PgPool) -> Result<(), String> {
    let r = crate::db::migrations::run_pg_migrations(pool).await;
    if r.is_err() {
        let _ = sqlx::raw_sql("ROLLBACK").execute(pool).await;
    }
    r
}

/// La bascule SQLite → PostgreSQL (`migrate_sqlite_to_pg`, sans les données)
/// puis le démarrage qui suit.
async fn bascule(pool: &PgPool) -> (Result<(), String>, Vec<String>) {
    sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
        .execute(pool)
        .await
        .expect("PG_FULL_SCHEMA");
    let mut r = migrer(pool).await;
    if let Err(e) = sqlx::raw_sql(crate::db::pg_migrate::PG_NORMALIZE_FILE_MTIME)
        .execute(pool)
        .await
    {
        r = r.and(Err(format!("PG_NORMALIZE_FILE_MTIME : {e}")));
    }
    let mut echecs = ensure_schema(pool).await;
    if let Err(e) = migrer(pool).await {
        r = r.and(Err(format!("second démarrage : {e}")));
    }
    echecs.extend(ensure_schema(pool).await);
    (r, echecs)
}

type Photo = BTreeMap<String, String>;

/// Colonnes et déclencheurs du schéma COURANT.
async fn photo(pool: &PgPool) -> Photo {
    let mut p = Photo::new();
    for l in sqlx::query(
        "SELECT table_name::text AS t, column_name::text AS c, data_type::text AS d, \
                is_nullable::text AS n, COALESCE(column_default::text, '') AS def \
           FROM information_schema.columns \
          WHERE table_schema = current_schema()",
    )
    .fetch_all(pool)
    .await
    .expect("colonnes")
    {
        let (t, c, d, n, def): (String, String, String, String, String) =
            (l.get("t"), l.get("c"), l.get("d"), l.get("n"), l.get("def"));
        p.insert(format!("{t}.{c}"), format!("{d} null={n} défaut={def}"));
    }
    for l in sqlx::query(
        "SELECT cl.relname::text AS t, tg.tgname::text AS g \
           FROM pg_trigger tg \
           JOIN pg_class cl ON cl.oid = tg.tgrelid \
           JOIN pg_namespace ns ON ns.oid = cl.relnamespace \
          WHERE NOT tg.tgisinternal AND ns.nspname = current_schema()",
    )
    .fetch_all(pool)
    .await
    .expect("déclencheurs")
    {
        let (t, g): (String, String) = (l.get("t"), l.get("g"));
        p.insert(format!("{t} déclencheur {g}"), "présent".into());
    }
    p
}

fn ecarts(temoin: &Photo, cible: &Photo) -> Vec<String> {
    let mut v = Vec::new();
    for (k, a) in temoin {
        match cible.get(k) {
            Some(b) if b == a => {}
            Some(b) => v.push(format!("  {k} : témoin `{a}`, avec voisin `{b}`")),
            None => v.push(format!("  {k} : témoin `{a}`, ABSENT avec voisin")),
        }
    }
    for (k, b) in cible {
        if !temoin.contains_key(k) {
            v.push(format!("  {k} : absent du témoin, `{b}` avec voisin"));
        }
    }
    v
}

#[derive(Clone, Copy)]
enum Monte {
    /// `PG_FULL_SCHEMA` seul : la copie tout-TEXT d'une bascule jamais migrée.
    ToutTexte,
    Natif,
    Bascule,
}

async fn monter(pool: &PgPool, m: Monte) -> (Result<(), String>, Vec<String>) {
    match m {
        Monte::ToutTexte => {
            let r = sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|e| format!("PG_FULL_SCHEMA : {e}"));
            (r, Vec::new())
        }
        Monte::Natif => demarrage_natif(pool).await,
        Monte::Bascule => bascule(pool).await,
    }
}

async fn epreuve(url: String, suffixe: &'static str, voisin: Monte, cible: Monte) {
    let base = format!("tune_gardes_5003_{suffixe}");
    let base_temoin = format!("tune_gardes_5003_{suffixe}_temoin");

    // Le témoin : la même naissance, sans voisin.
    base_vierge(&url, &base_temoin, &[SCHEMA]).await;
    let p = pool_sur(&url, &base_temoin, SCHEMA).await;
    let (r_temoin, echecs_temoin) = monter(&p, cible).await;
    r_temoin.expect("le témoin sans voisin doit monter — le test ne vérifierait rien");
    let temoin = photo(&p).await;
    p.close().await;
    assert!(
        temoin.keys().filter(|k| k.starts_with("tracks.")).count() > 20,
        "le témoin n'a pas monté `tracks` — le test ne vérifierait rien"
    );

    // La base à voisin : `leurre` créé AVANT `cible`.
    base_vierge(&url, &base, &[VOISIN, SCHEMA]).await;
    let p = pool_sur(&url, &base, VOISIN).await;
    let (r_voisin, _) = monter(&p, voisin).await;
    r_voisin.expect("le voisin doit monter");
    let photo_voisin = photo(&p).await;
    p.close().await;
    assert!(
        photo_voisin.keys().any(|k| k.starts_with("tracks.")),
        "le voisin n'a pas de `tracks` — il ne leurrerait aucune garde"
    );

    let p = pool_sur(&url, &base, SCHEMA).await;
    let (r, echecs) = monter(&p, cible).await;
    r.unwrap_or_else(|e| {
        panic!(
            "Les migrations échouent quand un schéma VOISIN porte les mêmes \
             tables : une garde lit `information_schema` sans `table_schema = \
             current_schema()` et agit sur la table du voisin (#5003).\n{e}"
        )
    });
    let avec_voisin = photo(&p).await;
    p.close().await;
    let nouveaux: Vec<&String> = echecs
        .iter()
        .filter(|e| !echecs_temoin.contains(e))
        .collect();
    assert!(
        nouveaux.is_empty(),
        "`ensure_schema` échoue à cause du voisin (garde sans filtre de \
         schéma, #5003) :\n{nouveaux:#?}"
    );
    let v = ecarts(&temoin, &avec_voisin);
    assert!(
        v.is_empty(),
        "Un schéma VOISIN change le schéma que Tune reçoit : une garde de \
         migration lit `information_schema` sans `table_schema = \
         current_schema()` et saute (ou tente à tort) une conversion (#5003).\n{}",
        v.join("\n")
    );
}

/// Lance l'épreuve dans une tâche, supprime les bases QUOI QU'IL ARRIVE, puis
/// rend l'éventuelle panique.
async fn epreuve_nettoyee(suffixe: &'static str, voisin: Monte, cible: Monte) {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    let resultat = tokio::spawn(epreuve(url.clone(), suffixe, voisin, cible)).await;
    supprimer_base(&url, &format!("tune_gardes_5003_{suffixe}")).await;
    supprimer_base(&url, &format!("tune_gardes_5003_{suffixe}_temoin")).await;
    if let Err(e) = resultat {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// Installation neuve à côté d'une copie tout-TEXT (bascule restaurée dans un
/// autre schéma) : les gardes `IF EXISTS … 'text'` ne doivent pas tirer.
#[tokio::test(flavor = "multi_thread")]
async fn pg_5003_natif_a_cote_d_un_voisin_tout_texte() {
    epreuve_nettoyee("natif", Monte::ToutTexte, Monte::Natif).await;
}

/// Base basculée depuis SQLite à côté d'une installation native déjà BIGINT :
/// les gardes `SELECT data_type INTO` ne doivent pas sauter la conversion.
#[tokio::test(flavor = "multi_thread")]
async fn pg_5003_bascule_a_cote_d_un_voisin_natif() {
    epreuve_nettoyee("bascule", Monte::Natif, Monte::Bascule).await;
}

/// Retire les commentaires `-- …` d'un script : un commentaire qui CITE
/// `information_schema.columns` (celui de la 072, par exemple) n'est pas une
/// requête.
fn sans_commentaires(sql: &str) -> String {
    sql.lines()
        .map(|l| l.split_once("--").map(|(avant, _)| avant).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Chaque lecture d'`information_schema.columns|tables` d'une migration
/// numérotée, d'`ensure_schema` ou de la bascule porte `table_schema` dans
/// SA clause, c'est-à-dire avant le `;` ou la `)` qui la ferme — pas quelque
/// part ailleurs dans le fichier. Sans base : il tourne dans toute exécution
/// de `cargo test --features postgres`.
#[test]
fn pg_5003_chaque_lecture_d_information_schema_filtre_le_schema() {
    let mut sources: Vec<(String, &str)> = crate::db::migrations::PG_MIGRATIONS
        .iter()
        .map(|(v, nom, sql)| (format!("{v:03}_{nom}.sql"), *sql))
        .collect();
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        sources.push(("ensure_schema".into(), sql));
    }
    sources.push((
        "PG_NORMALIZE_FILE_MTIME".into(),
        crate::db::pg_migrate::PG_NORMALIZE_FILE_MTIME,
    ));

    let mut lectures = 0usize;
    let mut fautives = Vec::new();
    for (nom, sql) in &sources {
        let sql = sans_commentaires(sql);
        for vue in ["information_schema.columns", "information_schema.tables"] {
            for (i, _) in sql.match_indices(vue) {
                lectures += 1;
                let reste = &sql[i + vue.len()..];
                let fin = reste.find([';', ')']).unwrap_or(reste.len());
                if !reste[..fin].contains("table_schema") {
                    let ligne = sql[..i].lines().count() + 1;
                    fautives.push(format!("  {nom}, ligne {ligne} : {vue}"));
                }
            }
        }
    }
    // Garde-fou du garde-fou : une recherche devenue muette rendrait vert.
    assert!(
        lectures >= 25,
        "seulement {lectures} lectures d'information_schema trouvées — la \
         recherche ne voit plus les gardes"
    );
    assert!(
        fautives.is_empty(),
        "Lectures d'information_schema sans `table_schema = current_schema()` : \
         une table homonyme d'un autre schéma les trompe (#5003).\n{}",
        fautives.join("\n")
    );
}
