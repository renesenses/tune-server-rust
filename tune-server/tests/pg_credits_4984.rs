//! Les crédits de piste sur une VRAIE base PostgreSQL (#4984).
//!
//! `track_credits.track_id` est BIGINT sur PostgreSQL — dès
//! `001_initial_schema.sql`, et `012_integer_id_columns.sql` a converti les
//! bases migrées depuis SQLite. Le code liait pourtant l'identifiant de piste
//! en TEXTE, sur la foi d'un « schéma miroir » révolu. SQLite l'avalait ;
//! PostgreSQL refusait, et le journal du .15 le montrait :
//!
//! ```text
//! ERROR: column "track_id" is of type bigint but expression is of type text
//!   → aucun crédit n'était JAMAIS écrit (INSERT de ecrire_credits_piste)
//! ERROR: operator does not exist: bigint = text
//!   → DELETE de purge (erreur avalée) et GET /library/tracks/{id}/credits en 500
//! ```
//!
//! Le bandeau « Server error: pg query_many … bigint = text » est ce que
//! l'interface du .42 affichait en lisant le .15 (25/09/2026).
//!
//! Ce banc écrit des crédits par le chemin de production
//! (`credits_release::ecrire_credits_piste`), DEUX fois pour exiger que la
//! purge ait lieu, puis les relit par le vrai routeur : crédits de la piste et
//! crédits de l'artiste. Même doctrine que `pg_routes_serveur.rs` : variable
//! ABSENTE ⇒ saut annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use tune_core::db::backend::{SqlValue, ToSqlValue};
use tune_core::metadata::credits_mb::LigneCredit;
use tune_core::metadata::credits_release::ecrire_credits_piste;
use tune_server::state::AppState;

/// Suffixe propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide rien et on ne compte que nos lignes.
const MARQUE: &str = "credits-4984";

fn url_pg() -> Option<String> {
    std::env::var("TUNE_TEST_PG_URL").ok()
}

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(state: &AppState) {
    let b = &state.backend;
    for sql in [
        format!(
            "DELETE FROM track_credits WHERE track_id IN \
             (SELECT id FROM tracks WHERE title = '{MARQUE}')"
        ),
        format!("DELETE FROM tracks WHERE title = '{MARQUE}'"),
        format!("DELETE FROM artists WHERE name = '{MARQUE}'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

fn lignes(nom_artiste: &str) -> Vec<LigneCredit> {
    vec![
        LigneCredit {
            artist_name: nom_artiste.to_string(),
            role: "performer".into(),
            instrument: Some("piano".into()),
            artist_mbid: None,
        },
        LigneCredit {
            artist_name: format!("{MARQUE}-invite"),
            role: "producer".into(),
            instrument: None,
            artist_mbid: None,
        },
    ]
}

async fn get_json(state: &AppState, route: &str) -> serde_json::Value {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let corps = axum::body::to_bytes(reponse.into_body(), 256 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&corps).to_string();
    assert!(
        statut.is_success(),
        "{route} → {statut} sur PostgreSQL (#4984) : {texte}"
    );
    serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} : JSON illisible ({e}) : {texte}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_4984_les_credits_s_ecrivent_se_purgent_et_se_relisent() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let b = state.backend.clone();

    let artist_id = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&MARQUE as &dyn ToSqlValue],
        )
        .expect("insertion de l'artiste");
    let track_id = b
        .execute_returning_id(
            "INSERT INTO tracks (title) VALUES (?)",
            &[&MARQUE as &dyn ToSqlValue],
        )
        .expect("insertion de la piste");

    // Deux écritures des MÊMES lignes : sans purge, la seconde doublerait.
    for passe in 1..=2 {
        let ecrites = ecrire_credits_piste(&b, track_id, &lignes(MARQUE));
        assert_eq!(
            ecrites, 2,
            "passe {passe} : ecrire_credits_piste n'a écrit que {ecrites} crédit(s) sur 2 — \
             sur PostgreSQL l'INSERT liait track_id en texte dans une colonne bigint (#4984)"
        );
    }
    let en_base = match b
        .query_one(
            "SELECT COUNT(*) FROM track_credits WHERE track_id = ?",
            &[&track_id as &dyn ToSqlValue],
        )
        .expect("comptage des crédits")
        .expect("une ligne de comptage")[0]
    {
        SqlValue::Int(n) => n,
        ref autre => panic!("COUNT(*) n'a pas rendu un entier : {autre:?}"),
    };
    assert_eq!(
        en_base, 2,
        "{en_base} crédits en base après deux écritures identiques : la purge \
         (DELETE … WHERE track_id = ?) n'a pas eu lieu sur PostgreSQL (#4984)"
    );

    let piste = get_json(
        &state,
        &format!("/api/v1/library/tracks/{track_id}/credits"),
    )
    .await;
    assert_eq!(
        piste.as_array().map(Vec::len),
        Some(2),
        "GET des crédits de la piste : {piste}"
    );

    let artiste = get_json(
        &state,
        &format!("/api/v1/library/artists/{artist_id}/credits"),
    )
    .await;
    assert!(
        artiste.as_array().is_some_and(|l| !l.is_empty()),
        "GET des crédits de l'artiste : aucun crédit rendu alors que la fiche est créditée : {artiste}"
    );

    menage(&state);
}
