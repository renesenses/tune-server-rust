//! Le schéma PostgreSQL est écrit DEUX fois. Ce test refuse qu'elles divergent.
//!
//! Une base PostgreSQL peut naître de deux façons, et une seule des deux est
//! testée d'habitude :
//!
//! | naissance | ce qui monte le schéma | qui l'emprunte |
//! |---|---|---|
//! | base neuve, ou base existante au démarrage | les scripts `.sql` numérotés, **puis** le rattrapage de `ensure_schema` | tout serveur PostgreSQL |
//! | bascule depuis SQLite | `PG_FULL_SCHEMA`, d'un bloc | l'assistant de migration |
//!
//! Le premier chemin en compte donc DEUX, et les deux comptent : une colonne
//! peut légitimement n'arriver que par `ENSURE_COLUMNS`, sans migration
//! numérotée. Ce test monte les deux avant de comparer — modéliser la base
//! existante par les seuls scripts ferait crier sur une vingtaine de colonnes
//! qui ne manquent nulle part.
//!
//! Rien n'oblige ces deux chemins à rester d'accord. `run_migrations` ne prend
//! qu'un `SqliteDb` et PostgreSQL a sa liste séparée ; ajouter une colonne au
//! `CREATE TABLE` de `pg_migrate.rs` sans écrire la migration numérotée
//! correspondante compile, passe la revue, et ne se voit nulle part.
//!
//! C'est exactement ce qui est arrivé aux trois colonnes du chantier CUE
//! (#2111). Elles n'ont jamais atteint aucune base existante. Le défaut est
//! resté latent des mois parce qu'aucune requête ne les nommait encore, et il
//! ne s'est signalé que de biais — une migration ultérieure qui les lisait a
//! fait échouer le job qui applique les scripts sur une base nue.
//!
//! ## Pourquoi deux bases réelles, et pas un parseur
//!
//! On pourrait lire `PG_FULL_SCHEMA` au texte et en extraire les colonnes. Ce
//! serait plus court, et faux le jour où le SQL prend une forme que le parseur
//! ne reconnaît pas : il rendrait alors MOINS de colonnes attendues, donc
//! moins d'écarts, donc un test **vert par ignorance**. Un garde-fou qui
//! s'affaiblit en silence est pire que pas de garde-fou — il porte la
//! signature d'une vérification qui n'a pas eu lieu.
//!
//! On monte donc les deux schémas dans deux vraies bases jetables et on
//! compare `information_schema`. C'est PostgreSQL lui-même qui répond, et il
//! ne peut pas se tromper sur ce qu'il vient de créer.
//!
//! ## Portée
//!
//! Le test ne signale que les colonnes **présentes dans le schéma neuf et
//! absentes des scripts** — le sens qui casse une base existante. L'inverse
//! (une colonne que seuls les scripts ajoutent) est légitime : plusieurs
//! migrations corrigent des types ou ajoutent des index sans que le schéma
//! neuf ait à les rejouer, puisqu'il naît déjà correct.
//!
//! Les tables absentes d'un côté ne sont pas comparées non plus : le schéma
//! neuf ne crée que les tables que la copie SQLite→PG alimente.
//!
//! ## Comment il tourne
//!
//! Comme les autres tests PostgreSQL : sauté sans `TUNE_TEST_PG_URL`, donc
//! absent d'un `cargo test` par défaut. La CI le lance sur son PostgreSQL 16.

#![cfg(all(test, feature = "postgres"))]

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Connection, PgConnection, Row};

/// Les deux bases jetables. Recréées à chaque exécution — un reste d'une
/// exécution précédente rendrait le résultat dépendant de l'historique.
const BASE_SCRIPTS: &str = "tune_parite_scripts";
const BASE_NEUVE: &str = "tune_parite_neuve";

/// Remplace le nom de base dans une URL `postgresql://…/nom`.
///
/// Volontairement littéral plutôt que passé par un analyseur d'URL : la seule
/// forme qu'on reçoit est celle que la CI écrit, et une dépendance de plus pour
/// couper sur un `/` ne se justifie pas. La query string est conservée.
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

/// Ouvre une connexion unique (pas un pool) : chaque base est montée puis lue
/// dans la foulée, et un pool n'apporterait ici que le risque de changer de
/// session entre deux ordres.
async fn connexion(url: &str) -> PgConnection {
    PgConnection::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connexion à {url} impossible : {e}"))
}

/// Détruit puis recrée la base, et y installe `unaccent` comme le fait la CI
/// avant d'appliquer les scripts.
async fn base_vierge(maintenance: &mut PgConnection, nom: &str, url_racine: &str) -> PgConnection {
    // `DROP DATABASE` refuse de s'exécuter dans une transaction, d'où raw_sql.
    //
    // `AssertSqlSafe` : `nom` ne vient d'aucune entrée, c'est l'une des deux
    // constantes déclarées en tête de ce fichier. Le nom d'une base ne peut de
    // toute façon pas être passé en paramètre lié — PostgreSQL n'accepte pas de
    // placeholder dans un ordre DDL.
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

/// Toutes les colonnes du schéma `public`, groupées par table.
async fn colonnes(c: &mut PgConnection) -> BTreeMap<String, BTreeSet<String>> {
    let lignes = sqlx::query(
        "SELECT table_name, column_name \
         FROM information_schema.columns \
         WHERE table_schema = 'public'",
    )
    .fetch_all(c)
    .await
    .expect("lecture d'information_schema");

    let mut par_table: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for l in lignes {
        let table: String = l.get("table_name");
        let colonne: String = l.get("column_name");
        par_table.entry(table).or_default().insert(colonne);
    }
    par_table
}

/// Le schéma neuf de `pg_migrate.rs` et celui des scripts numérotés doivent
/// s'accorder sur toute table qu'ils déclarent tous les deux.
///
/// En cas d'écart, le message nomme la table, la colonne, et le fichier à
/// écrire — c'est ce qui a manqué pendant des mois sur les colonnes CUE.
#[tokio::test]
async fn parite_du_schema_pg() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absent — parite_du_schema_pg sautée");
        return;
    };

    let mut maintenance = connexion(&url).await;

    // Base A : le parcours d'un serveur existant — les scripts, dans l'ordre.
    let mut a = base_vierge(&mut maintenance, BASE_SCRIPTS, &url).await;
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TIMESTAMPTZ DEFAULT now(),
            name TEXT NOT NULL
        )",
    )
    .execute(&mut a)
    .await
    .expect("schema_version");
    for (version, nom, sql) in crate::db::migrations::PG_MIGRATIONS {
        sqlx::raw_sql(*sql)
            .execute(&mut a)
            .await
            .unwrap_or_else(|e| panic!("migration {version:03}_{nom} : {e}"));
    }
    // …puis le rattrapage que `ensure_schema` rejoue à CHAQUE démarrage.
    //
    // Sans lui, la base A ne représenterait pas une base existante mais une
    // fiction : `ENSURE_COLUMNS` soigne au vol une vingtaine de colonnes que
    // les scripts n'ajoutent pas, et les compter comme manquantes ferait crier
    // ce test sur des défauts qui n'existent pas. Une base réelle a bien ces
    // colonnes — simplement pas par une migration numérotée.
    //
    // Les deux voies sont donc acceptées, et c'est volontaire : ce qu'on veut
    // interdire n'est pas une façon particulière d'ajouter la colonne, c'est
    // qu'elle n'arrive JAMAIS.
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        sqlx::raw_sql(*sql)
            .execute(&mut a)
            .await
            .unwrap_or_else(|e| panic!("rattrapage ensure_schema `{sql}` : {e}"));
    }

    // Base B : le schéma neuf, d'un bloc.
    let mut b = base_vierge(&mut maintenance, BASE_NEUVE, &url).await;
    sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
        .execute(&mut b)
        .await
        .expect("PG_FULL_SCHEMA");

    let scripts = colonnes(&mut a).await;
    let neuf = colonnes(&mut b).await;

    // Garde-fou du garde-fou : si l'une des deux lectures rendait peu de
    // choses — mauvaise base, schéma vide, requête muette — la comparaison
    // serait vide et le test vert sans avoir rien vérifié. `tracks` est la
    // table la plus large des deux côtés ; l'exiger ancre le test dans le réel.
    assert!(
        scripts.get("tracks").is_some_and(|c| c.len() > 20),
        "les scripts n'ont pas monté `tracks` — le test ne vérifiait rien"
    );
    assert!(
        neuf.get("tracks").is_some_and(|c| c.len() > 20),
        "le schéma neuf n'a pas monté `tracks` — le test ne vérifiait rien"
    );

    let mut ecarts: Vec<String> = Vec::new();
    for (table, attendues) in &neuf {
        let Some(obtenues) = scripts.get(table) else {
            continue; // table absente des scripts : hors portée, cf. en-tête
        };
        for colonne in attendues.difference(obtenues) {
            ecarts.push(format!("  {table}.{colonne}"));
        }
    }

    assert!(
        ecarts.is_empty(),
        "Colonnes présentes dans le schéma PostgreSQL NEUF (pg_migrate.rs) mais \
         qu'aucune migration numérotée n'ajoute :\n{}\n\n\
         Une base PostgreSQL existante ne les recevra JAMAIS : `CREATE TABLE` ne \
         s'applique qu'à une base neuve.\n\
         Écrire `tune-core/migrations/postgres/NNN_….sql` avec \
         `ADD COLUMN IF NOT EXISTS`, l'inscrire dans `PG_MIGRATIONS`, et l'ajouter \
         au bloc de rattrapage de `PG_FULL_SCHEMA` si la copie SQLite→PG la lit.\n\
         Une colonne s'ajoute à QUATRE endroits, pas trois (#2111).",
        ecarts.join("\n")
    );
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
