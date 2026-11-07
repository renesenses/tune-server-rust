//! #8077 — les apports Qobuz sur une VRAIE base PostgreSQL (migration 108),
//! mêmes scénarios que `apports_qobuz_8077.rs` (`apports_qobuz_8077/commun.rs`).
//!
//! Même doctrine que `pg_requete_avancee_7872.rs` : variable
//! `TUNE_TEST_PG_URL` ABSENTE ⇒ saut annoncé ; POSÉE mais injoignable ⇒ le
//! test TOMBE. Chaque scénario porte sa marque dans les noms ; on ne vide que
//! nos lignes.

#![cfg(feature = "postgres")]

#[path = "apports_qobuz_8077/commun.rs"]
mod commun;

use commun::Etat;

fn etat(url: &str) -> Etat {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    Etat::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(state: &Etat, marque: &str) {
    let b = &state.backend;
    for sql in [
        format!(
            "DELETE FROM apports_service WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{marque} %')"
        ),
        format!(
            "DELETE FROM apports_service_albums WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{marque} %')"
        ),
        format!(
            "DELETE FROM track_credits WHERE track_id IN \
             (SELECT t.id FROM tracks t JOIN albums a ON a.id = t.album_id WHERE a.title LIKE '{marque} %')"
        ),
        format!(
            "DELETE FROM tracks WHERE album_id IN (SELECT id FROM albums WHERE title LIKE '{marque} %')"
        ),
        format!("DELETE FROM albums WHERE title LIKE '{marque} %'"),
        format!("DELETE FROM artists WHERE name LIKE '{marque} %'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_apports_qobuz_8077() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absent — pg_apports_qobuz_8077 sauté");
        return;
    };
    let version: i64 = etat(&url)
        .backend
        .query_one("SELECT MAX(version) FROM schema_version", &[])
        .unwrap()
        .and_then(|r| r[0].as_i64())
        .unwrap();
    assert!(
        version >= 108,
        "la migration PG 108 n'est pas jouée (dernière : {version})"
    );

    for (marque, n) in [
        ("q8077a", 0),
        ("q8077b", 1),
        ("q8077c", 2),
        ("q8077d", 3),
        ("q8077e", 4),
    ] {
        let s = etat(&url);
        menage(&s, marque);
        match n {
            0 => commun::upc_complete_sans_ecraser_puis_retrait(s.clone(), marque).await,
            1 => commun::titre_douteux_non_applique_puis_confirme(s.clone(), marque).await,
            2 => commun::titre_pistes_differentes_non_applique(s.clone(), marque).await,
            3 => commun::titre_sur_applique(s.clone(), marque).await,
            _ => commun::sans_compte_aucun_appel(s.clone(), marque).await,
        }
        menage(&s, marque);
    }
}
