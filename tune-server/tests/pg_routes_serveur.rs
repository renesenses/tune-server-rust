//! Les requêtes SQL de `tune-server` sur une VRAIE base PostgreSQL (#3123).
//!
//! `test-postgres.yml` ne compilait que `tune-core`. Les **419** littéraux SQL
//! écrits dans `tune-server/src` (54 fichiers, comptés le 01/09/2026) n'avaient
//! donc **jamais** été exécutés sur PostgreSQL : sur aucune branche, avec aucune
//! étiquette, jamais. C'est ce qui a laissé passer #2860 puis #2441, une release
//! plus tard — `total = 0` rend `NULL` sur SQLite et lève `division by zero` sur
//! PostgreSQL, et la section entière de l'accueil serait partie vide chez tout
//! utilisateur PG.
//!
//! Les deux fois, le correctif a consisté à DESCENDRE la requête dans
//! `tune-core` pour qu'une porte l'exécute. Ce fichier retire cette obligation :
//! il monte un `AppState` sur le moteur PostgreSQL — le chemin exact de la
//! production, `TuneConfig::database_url` en `postgres://…` — et fait passer de
//! vraies requêtes HTTP par le vrai routeur. Une requête peut donc rester là où
//! elle est utile et être quand même jouée sur les deux moteurs.
//!
//! **Bibliothèque VIDE, délibérément** : c'est l'état où un agrégat divise par
//! zéro, et c'est celui d'un serveur qui vient d'être installé.
//!
//! ⚠️ Différence assumée avec `pg_or_skip!` (`tune-core/src/db/postgres_e2e.rs`) :
//! là-bas, une connexion qui ÉCHOUE rend `None` et le test se saute en silence,
//! si bien qu'un banc mal branché s'affiche vert. Ici, la variable ABSENTE saute
//! (le `cargo test` ordinaire n'a pas de base), mais une variable POSÉE dont la
//! connexion échoue fait TOMBER le test.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use tune_server::state::AppState;

/// Les routes de lecture dont le handler porte du SQL écrit dans `tune-server`
/// — `routes/history.rs` et `routes/dashboard.rs`, les deux voisines directes
/// des requêtes de #2860 et #2441.
///
/// **Inventaire, pas échantillon** : on n'en retire pas. Le test refuse d'en
/// voir moins que [`MINIMUM_DE_ROUTES`], pour qu'une liste vidée par mégarde
/// rougisse au lieu de passer à vide.
const ROUTES_DE_LECTURE: &[&str] = &[
    "/api/v1/history",
    "/api/v1/history/top-tracks",
    "/api/v1/history/top-artists",
    "/api/v1/history/top-albums",
    "/api/v1/history/dashboard",
    "/api/v1/dashboard/stats",
    "/api/v1/dashboard/top-artists",
    "/api/v1/dashboard/top-albums",
    "/api/v1/dashboard/top-tracks",
    "/api/v1/dashboard/genre-breakdown",
    "/api/v1/dashboard/listening-history",
    "/api/v1/dashboard/wrapped",
];

/// Plancher du détecteur : une sonde qui ne sonde plus rien doit échouer, pas
/// réussir (même patron que le garde de #3152).
const MINIMUM_DE_ROUTES: usize = 12;

/// Les tables que les routes sondées lisent, vidées avant l'épreuve.
///
/// Ce n'est pas de la propreté : c'est **l'état sous test**. Les étapes
/// `tune-core` de `test-postgres.yml` qui précèdent laissent des lignes dans
/// `listen_history` (leurs `reset_schema` nettoient AVANT, pas après), et une
/// table non vide masque précisément le défaut qu'on garde ici — `COUNT(*) = 2`
/// ne divise par zéro nulle part. Mesuré : sans ce vidage, le sabotage de
/// contre-épreuve ne rougit pas.
const TABLES_VIDEES: &[&str] = &["listen_history", "tracks", "albums", "artists"];

/// L'URL du banc PostgreSQL, ou `None` quand la variable n'est pas posée.
fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

/// Monte l'état du serveur sur PostgreSQL. `AppState::new` applique lui-même
/// les migrations PG, comme au démarrage réel.
fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    // Pas de `ok()?` ici : une connexion qui échoue doit ROUGIR, jamais sauter.
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_3123_routes_de_lecture_sur_postgresql() {
    assert!(
        ROUTES_DE_LECTURE.len() >= MINIMUM_DE_ROUTES,
        "la liste des routes sondées est tombée à {} (< {MINIMUM_DE_ROUTES}) : \
         le détecteur passerait à vide",
        ROUTES_DE_LECTURE.len()
    );

    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);

    // Bibliothèque vide : l'état d'un serveur qui vient d'être installé, et
    // celui où un agrégat divise par zéro. En série (un seul test dans ce
    // binaire) — les `TRUNCATE … CASCADE` s'interbloquent en parallèle.
    for table in TABLES_VIDEES {
        state
            .backend
            .execute(
                &format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE"),
                &[],
            )
            .unwrap_or_else(|e| panic!("vidage de {table} sur PostgreSQL : {e}"));
    }

    let mut echecs = Vec::new();
    for route in ROUTES_DE_LECTURE {
        let app: Router = tune_server::routes::router(state.clone());
        let reponse = app
            .oneshot(Request::get(*route).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let statut = reponse.status();
        // 2xx exigé, et pas seulement « pas 5xx » : un 404 (route déplacée) ou
        // un 401 (garde d'authentification) prouverait que la requête SQL n'a
        // jamais été atteinte, et le test passerait sans rien avoir exercé.
        if !statut.is_success() {
            let corps = axum::body::to_bytes(reponse.into_body(), 64 * 1024)
                .await
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            echecs.push(format!("{route} → {statut} : {corps}"));
        }
    }

    assert!(
        echecs.is_empty(),
        "routes de lecture en échec sur PostgreSQL :\n{}",
        echecs.join("\n")
    );
}
