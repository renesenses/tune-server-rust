//! Le rapport de diagnostic dit-il le VRAI moteur, et la VRAIE version de
//! schéma ? (#3182)
//!
//! ## Ce qui a échappé aux portes
//!
//! `routes/system/diagnostics.rs` portait deux suppositions :
//!
//! - `md.push_str(&format!("- Engine: sqlite\n"))` — un `format!` sans
//!   argument, donc une chaîne littérale. Toute installation PostgreSQL se
//!   déclarait SQLite dans son propre `diagnostic.md` ;
//! - `db_version` valait `if engine == Sqlite { … } else { 0 }`, donc
//!   « Migration version: 0 » sur PostgreSQL — ce qui ne se lit pas
//!   « inconnue » mais « base jamais migrée », sur une base de 77 291 pistes.
//!
//! Sur le ticket 71 de jfpaquet ces deux lignes ont failli faire écarter
//! #3181, une issue qui n'existe QUE parce que le moteur est PostgreSQL.
//!
//! Deux autres copies du même « sqlite » en dur vivaient à côté : le
//! `db_backend` de `/system/diagnostics`, lu dans un réglage `db_engine` que
//! RIEN n'écrit dans `settings` (le `unwrap_or("sqlite")` était la seule
//! branche jamais empruntée), et le `"engine": "sqlite"` de
//! `/system/admin/health`. `/system/database/status`, lui, annonçait bien
//! `postgres` mais comparait la version PG de la base au dernier numéro
//! SQLite du binaire : `up_to_date: false` à demeure.
//!
//! ## Pourquoi ce test mesure le TEXTE
//!
//! Une épreuve qui rappellerait `state.backend.engine()` pour décider ce
//! qu'elle attend ne garderait rien : elle RECOPIERAIT la condition du code au
//! lieu de la garder, et resterait verte si la ligne redevenait littérale.
//! Ce fichier lit donc le markdown rendu par
//! `GET /api/v1/system/bug-report/markdown`, en extrait la section
//! `## Database`, et exige ce qui y est écrit.
//!
//! ## Doctrine du saut
//!
//! Reprise mot pour mot de `pg_3181_sections_accueil.rs` : `TUNE_TEST_PG_URL`
//! ABSENTE saute l'épreuve PostgreSQL en l'ANNONÇANT (le `cargo test`
//! ordinaire n'a pas de base) ; une variable POSÉE dont la connexion échoue
//! fait TOMBER le test — `AppState::new` n'y est pas rattrapé. Un banc mal
//! branché doit rougir, jamais s'afficher vert.
//!
//! La contre-épreuve SQLite, elle, ne dépend d'aucune base externe et tourne
//! donc dans le `cargo test --workspace` ordinaire : sans elle, réparer
//! PostgreSQL en cassant SQLite passerait toutes les portes. C'est pourquoi
//! cette cible ne porte PAS de `required-features`.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

/// Le titre de la section sondée dans le markdown.
const SECTION: &str = "## Database";

/// L'URL du banc PostgreSQL, ou `None` quand la variable n'est pas posée.
#[cfg(feature = "postgres")]
fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

/// Le serveur sur SQLite en mémoire.
fn etat_sqlite() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

/// Le serveur sur PostgreSQL — le chemin exact de la production,
/// `TuneConfig::database_url` en `postgres://…`. `AppState::new` applique
/// lui-même les migrations PG, comme au démarrage réel.
#[cfg(feature = "postgres")]
fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    // Pas de `ok()?` ici : une connexion qui échoue doit ROUGIR, jamais sauter.
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

/// Le corps brut d'une route, avec son statut exigé 2xx : un 404 ou un 401
/// prouverait que le rapport n'a jamais été construit.
async fn texte_de(state: &AppState, route: &str) -> String {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 8 * 1024 * 1024)
        .await
        .expect("corps de reponse");
    let texte = String::from_utf8_lossy(&octets).into_owned();
    assert!(statut.is_success(), "{route} → {statut} : {texte}");
    texte
}

/// Le corps JSON d'une route.
async fn json_de(state: &AppState, route: &str) -> Value {
    let texte = texte_de(state, route).await;
    serde_json::from_str(&texte).unwrap_or_else(|e| panic!("{route} : corps non JSON ({e})"))
}

/// La section `## Database` du markdown, titre exclu, jusqu'au titre suivant.
///
/// L'extraction est stricte : une section absente fait tomber le test plutôt
/// que de rendre une chaîne vide, contre laquelle un `!contains("sqlite")`
/// serait trivialement vert.
fn section_database(markdown: &str) -> String {
    let debut = markdown
        .find(SECTION)
        .unwrap_or_else(|| panic!("section « {SECTION} » absente du rapport :\n{markdown}"));
    let apres = &markdown[debut + SECTION.len()..];
    let fin = apres.find("\n#").unwrap_or(apres.len());
    let section = apres[..fin].trim().to_string();
    assert!(
        !section.is_empty(),
        "section « {SECTION} » vide — rien à mesurer"
    );
    section
}

/// La valeur de `- Migration version: …` telle qu'elle est ÉCRITE.
///
/// Rend le texte brut, pas un nombre : c'est la distinction entre `0` et
/// `unknown` que ce test doit pouvoir voir, et un parseur qui rendrait `0`
/// pour les deux la ferait disparaître.
fn version_ecrite(section: &str) -> String {
    section
        .lines()
        .find_map(|l| l.trim().strip_prefix("- Migration version:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| panic!("aucune ligne « - Migration version: » dans :\n{section}"))
}

/// La valeur de `- Engine: …` telle qu'elle est ÉCRITE.
fn moteur_ecrit(section: &str) -> String {
    section
        .lines()
        .find_map(|l| l.trim().strip_prefix("- Engine:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| panic!("aucune ligne « - Engine: » dans :\n{section}"))
}

/// Ce qu'on exige d'un rapport, quel que soit le moteur : il nomme le moteur
/// ATTENDU, et sa version de schéma est un entier strictement positif.
///
/// `0` est refusé nommément. C'est la valeur que le code forçait, et une
/// assertion « c'est un nombre » l'aurait laissée passer.
async fn verifier_les_quatre_rapports(state: &AppState, moteur_attendu: &str) {
    // 1. Le markdown — ce que le testeur COLLE sur le forum.
    let markdown = texte_de(state, "/api/v1/system/bug-report/markdown").await;
    let section = section_database(&markdown);
    assert_eq!(
        moteur_ecrit(&section),
        moteur_attendu,
        "le rapport markdown annonce le mauvais moteur :\n{section}"
    );
    let version = version_ecrite(&section);
    assert_ne!(
        version, "0",
        "« Migration version: 0 » se lit « base jamais migrée » :\n{section}"
    );
    let version: i64 = version
        .parse()
        .unwrap_or_else(|_| panic!("version de schéma illisible : « {version} »\n{section}"));
    assert!(
        version >= 1,
        "version de schéma annoncée : {version}\n{section}"
    );

    // 2. Le même rapport en JSON, que le client lit.
    let rapport = json_de(state, "/api/v1/system/bug-report").await;
    assert_eq!(
        rapport["database"]["engine"], moteur_attendu,
        "bug-report JSON : {}",
        rapport["database"]
    );
    assert_eq!(
        rapport["database"]["migration_version"].as_i64(),
        Some(version),
        "le JSON et le markdown du MÊME rapport ne disent pas la même version"
    );

    // 3. `/system/diagnostics` — deux champs, tous deux nourris par le
    //    `db_backend` qui se lisait dans un réglage jamais écrit.
    let diag = json_de(state, "/api/v1/system/diagnostics").await;
    assert_eq!(diag["db_backend"], moteur_attendu, "diagnostics.db_backend");
    assert_eq!(
        diag["db"]["engine"], moteur_attendu,
        "diagnostics.db.engine"
    );
    assert_eq!(
        diag["db"]["migration_version"].as_i64(),
        Some(version),
        "diagnostics.db.migration_version"
    );

    // 4. `/system/database/status` — la comparaison qui rendait
    //    `up_to_date: false` à demeure sur PostgreSQL.
    let statut = json_de(state, "/api/v1/system/database/status").await;
    assert_eq!(statut["engine"], moteur_attendu, "database/status.engine");
    assert_eq!(
        statut["migration_version"].as_i64(),
        Some(version),
        "database/status.migration_version"
    );
    assert_eq!(
        statut["up_to_date"].as_bool(),
        Some(true),
        "une base que le serveur vient de migrer n'est pas « à jour » : {statut}"
    );

    // 5. `/system/admin/health` — la troisième copie du littéral.
    let sante = json_de(state, "/api/v1/system/admin/health").await;
    assert_eq!(
        sante["database"]["engine"], moteur_attendu,
        "admin/health.database.engine"
    );
}

/// **L'épreuve qui tranche.** Le rapport construit sur une VRAIE base
/// PostgreSQL doit dire `postgres`, et une version de schéma non nulle.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn sur_postgresql_le_rapport_annonce_postgres_et_sa_vraie_version() {
    let Some(url) = url_pg() else {
        eprintln!(
            "TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #3182 SAUTÉE \
             (elle est jouée par test-postgres.yml)"
        );
        return;
    };

    let pg = etat_postgres(&url);
    verifier_les_quatre_rapports(&pg, "postgres").await;

    // Et la version annoncée est bien celle que le binaire vient d'appliquer,
    // pas un nombre pris au hasard.
    let markdown = texte_de(&pg, "/api/v1/system/bug-report/markdown").await;
    let version: i32 = version_ecrite(&section_database(&markdown))
        .parse()
        .expect("version de schéma entière");
    assert_eq!(
        version,
        tune_core::db::migrations::pg_latest_version(),
        "la version annoncée n'est pas celle que `run_pg_migrations` vient \
         de poser dans `schema_version`"
    );

    // Le pilote annoncé suit le moteur, lui aussi.
    let diag = json_de(&pg, "/api/v1/system/diagnostics").await;
    assert_eq!(diag["rust_engines"]["db_engine"], "sqlx");
}

/// **La contre-épreuve.** Réparer PostgreSQL en cassant SQLite serait un
/// échange, pas une correction. Sans base externe : tourne dans le
/// `cargo test --workspace` ordinaire.
#[tokio::test(flavor = "multi_thread")]
async fn sur_sqlite_le_rapport_annonce_toujours_sqlite() {
    let sqlite = etat_sqlite();
    verifier_les_quatre_rapports(&sqlite, "sqlite").await;

    let diag = json_de(&sqlite, "/api/v1/system/diagnostics").await;
    assert_eq!(diag["rust_engines"]["db_engine"], "rusqlite");
}

/// Le rendu ne doit connaître que deux moteurs, et un rapport ne doit jamais
/// nommer l'autre dans sa section `## Database`.
///
/// Sans ce détecteur, un rendu qui écrirait le moteur DEUX fois — l'ancien
/// littéral laissé en place à côté du nouveau — passerait les assertions
/// ci-dessus, qui ne lisent que la PREMIÈRE ligne `- Engine:`.
#[tokio::test(flavor = "multi_thread")]
async fn la_section_database_ne_nomme_qu_un_seul_moteur() {
    let sqlite = etat_sqlite();
    let markdown = texte_de(&sqlite, "/api/v1/system/bug-report/markdown").await;
    let section = section_database(&markdown);
    assert_eq!(
        section
            .lines()
            .filter(|l| l.trim().starts_with("- Engine:"))
            .count(),
        1,
        "plusieurs lignes « - Engine: » dans :\n{section}"
    );
    assert!(
        !section.contains("postgres"),
        "un rapport SQLite nomme PostgreSQL :\n{section}"
    );
}
