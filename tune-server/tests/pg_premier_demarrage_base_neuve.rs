//! Le PREMIER démarrage d'une installation PostgreSQL native (chasse PG du
//! 25/09/2026).
//!
//! `PostgresDb::connect()` joue le DDL auto-réparateur (`ensure_schema`) PUIS
//! `run_pg_migrations()`. Sur une base VIDE, les `ALTER TABLE … ADD COLUMN IF
//! NOT EXISTS` d'`ensure_schema` échouent tous (`relation "listen_history" does
//! not exist`, journalisés puis avalés), et ce sont les scripts numérotés qui
//! créent les tables dans la foulée — SANS les colonnes que seul
//! `ENSURE_COLUMNS` apporte. La migration 049 avait déjà décrit ce mécanisme
//! pour deux colonnes ; il en restait quatorze. Mesuré sur PostgreSQL 16.15 :
//!
//! ```text
//! ERROR:  column h.album_id does not exist
//! ERROR:  column "source_id" does not exist
//! ERROR:  column "bio_source" does not exist
//! ```
//!
//! Conséquence, jusqu'au redémarrage suivant : l'historique d'écoute, « Continuer
//! l'écoute », les tops et la ventilation par genre rendent vide, les
//! biographies d'artiste et d'album échouent. Aucune porte ne le voyait :
//! `test-postgres.yml` applique les scripts par `psql` AVANT que le serveur ne
//! démarre, si bien qu'`ensure_schema` trouve toujours ses tables.
//!
//! Ce test monte une base NEUVE, démarre le serveur UNE fois, et exige les
//! colonnes ET une route qui s'en sert.

#![cfg(feature = "postgres")]

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use tune_server::state::AppState;

/// Les colonnes qu'aucun script numéroté n'apporte et qu'`ENSURE_COLUMNS`
/// seul pose — relevées sur une base neuve après UN démarrage, le 25/09/2026.
const COLONNES_DU_DDL_AUTO_REPARATEUR: &[(&str, &str)] = &[
    ("listen_history", "album_id"),
    ("listen_history", "source_id"),
    ("albums", "bio_source"),
    ("albums", "bio_source_url"),
    ("albums", "bio_license"),
    ("albums", "bio_lang"),
    ("albums", "bio_fetched_at"),
    ("artists", "bio_source"),
    ("artists", "bio_source_url"),
    ("artists", "bio_license"),
    ("artists", "bio_lang"),
    ("artists", "bio_fetched_at"),
    ("zones", "host"),
];

fn etat(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_premier_demarrage_d_une_base_neuve_a_toutes_ses_colonnes() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let racine = etat(&url);
    let nom = format!(
        "tune_premier_demarrage_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    racine
        .backend
        .execute(&format!("CREATE DATABASE {nom}"), &[])
        .expect("création de la base neuve");
    let prefixe = url.rsplit_once('/').unwrap().0;

    let mut ecarts = Vec::new();
    {
        // UN SEUL démarrage : c'est l'état d'un serveur qui vient d'être installé.
        let neuf = etat(&format!("{prefixe}/{nom}"));
        for (table, colonne) in COLONNES_DU_DDL_AUTO_REPARATEUR {
            let n = neuf
                .backend
                .query_one(
                    &format!(
                        "SELECT COUNT(*) FROM information_schema.columns \
                         WHERE table_schema = current_schema() \
                           AND table_name = '{table}' AND column_name = '{colonne}'"
                    ),
                    &[],
                )
                .unwrap()
                .and_then(|r| r.first().and_then(|v| v.as_i64()))
                .unwrap_or(0);
            if n != 1 {
                ecarts.push(format!(
                    "colonne absente au premier démarrage : {table}.{colonne}"
                ));
            }
        }

        // La conduite, pas seulement la forme : une écoute semée doit ressortir
        // de l'historique. Avant correctif, `h.album_id` manquait et la route
        // rendait une liste vide.
        neuf.backend
            .execute(
                "INSERT INTO listen_history (title, artist_name, album_title, source, listened_at) \
                 VALUES ('Moving', 'Kate Bush', 'The Kick Inside', 'local', '2026-09-25T09:00:00Z')",
                &[],
            )
            .expect("semence de l'historique");
        let app = tune_server::routes::router(neuf.clone());
        let rep = app
            .oneshot(Request::get("/api/v1/history").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let statut = rep.status();
        let corps = axum::body::to_bytes(rep.into_body(), 1 << 20)
            .await
            .unwrap();
        let texte = String::from_utf8_lossy(&corps).to_string();
        if !statut.is_success() || !texte.contains("Moving") {
            ecarts.push(format!(
                "GET /api/v1/history au premier démarrage : {statut} {texte}"
            ));
        }
    }

    let _ = racine
        .backend
        .execute(&format!("DROP DATABASE {nom} WITH (FORCE)"), &[]);
    assert!(ecarts.is_empty(), "base PostgreSQL neuve : {ecarts:#?}");
}
