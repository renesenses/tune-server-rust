//! #8026 — le rattrapage des crédits des balises et le rattachement des
//! lignes `balises` à leur fiche, sur une VRAIE base PostgreSQL.
//!
//! Jumeau des témoins SQLite
//! (`tune-core/src/metadata/rattrapage_credits_balises_8026_tests.rs`) :
//! `track_metadata.track_id` y est BIGINT ou TEXT selon l'histoire de la
//! base, les requêtes de la passe comparent en texte.
//!
//! Même doctrine que `pg_credits_4984.rs` : variable ABSENTE ⇒ saut annoncé ;
//! variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::metadata::rattrapage_credits_balises_8026::{
    CLE_RATTRAPAGE_CREDITS_BALISES_8026, rattacher_les_fiches, rattraper_les_credits_des_balises,
};
use tune_server::state::AppState;

const MARQUE: &str = "credits-balises-8026";

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

fn menage(b: &dyn DbBackend) {
    for sql in [
        format!(
            "DELETE FROM track_metadata WHERE CAST(track_id AS TEXT) IN \
             (SELECT CAST(id AS TEXT) FROM tracks WHERE title LIKE '{MARQUE}%')"
        ),
        format!(
            "DELETE FROM track_credits WHERE CAST(track_id AS TEXT) IN \
             (SELECT CAST(id AS TEXT) FROM tracks WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM tracks WHERE title LIKE '{MARQUE}%'"),
        format!("DELETE FROM artists WHERE name LIKE '%{MARQUE}%'"),
        format!("DELETE FROM settings WHERE key = '{CLE_RATTRAPAGE_CREDITS_BALISES_8026}'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

fn lignes(b: &dyn DbBackend, track_id: i64) -> Vec<String> {
    let mut v: Vec<String> = b
        .query_many(
            "SELECT COALESCE(source, 'NULL'), role, artist_name, COALESCE(instrument, ''), \
                    COALESCE(CAST(artist_id AS TEXT), '-') \
             FROM track_credits WHERE CAST(track_id AS TEXT) = ?",
            &[&track_id.to_string() as &dyn ToSqlValue],
        )
        .unwrap()
        .into_iter()
        .map(|r| {
            r.iter()
                .map(|c| c.as_string().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_8026_rattrapage_et_rattachement_des_credits_des_balises() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let b: &dyn DbBackend = state.backend.as_ref();
    menage(b);

    let nom_fiche = format!("Günter Hermanns {MARQUE}");
    let fiche = b
        .execute_returning_id(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom_fiche as &dyn ToSqlValue],
        )
        .expect("fiche");
    let titre = format!("{MARQUE}-piste");
    let piste = b
        .execute_returning_id(
            "INSERT INTO tracks (title) VALUES (?)",
            &[&titre as &dyn ToSqlValue],
        )
        .expect("piste");
    let ingenieur = format!("gunter hermanns {MARQUE}");
    for (cle, valeur) in [
        ("composer", "Richard Rodgers".to_string()),
        ("performer", "Bill Evans (piano)".to_string()),
        ("engineer", ingenieur.clone()),
        ("label", "United Artists".to_string()),
    ] {
        b.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
            &[
                &piste as &dyn ToSqlValue,
                &cle as &dyn ToSqlValue,
                &valeur as &dyn ToSqlValue,
            ],
        )
        .unwrap_or_else(|e| panic!("métadonnée {cle} : {e}"));
    }
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, position, source) \
         VALUES (?, 'Alan Douglas', 'producer', 0, 'musicbrainz')",
        &[&piste as &dyn ToSqlValue],
    )
    .unwrap();

    let bilan = rattraper_les_credits_des_balises(&state.backend).expect("rattrapage sur PG");
    assert!(!bilan.deja_fait && bilan.lignes >= 3, "{bilan:?}");
    assert_eq!(
        lignes(b, piste),
        vec![
            "balises|composer|Richard Rodgers||-".to_string(),
            format!("balises|engineer|{ingenieur}||-"),
            "balises|performer|Bill Evans|piano|-".to_string(),
            "musicbrainz|producer|Alan Douglas||-".to_string(),
        ],
        "#8026 — la piste déjà scannée n'a pas reçu ses crédits de balises sur PG"
    );
    assert!(
        rattraper_les_credits_des_balises(&state.backend)
            .unwrap()
            .deja_fait,
        "le marqueur n'a pas été posé"
    );

    rattacher_les_fiches(&state.backend).expect("rattachement sur PG");
    assert!(
        lignes(b, piste).contains(&format!("balises|engineer|{ingenieur}||{fiche}")),
        "#8026 — la ligne des balises n'a pas rejoint sa fiche sur PG : {:?}",
        lignes(b, piste)
    );

    menage(b);
}
