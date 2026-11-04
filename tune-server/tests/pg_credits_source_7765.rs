//! La source des crédits de piste sur une VRAIE base PostgreSQL (#7765).
//!
//! Jumeau des témoins SQLite (`tune-core/src/metadata/credits_source_7765_tests.rs`
//! et `tune-server/src/routes/library/credits.rs`) :
//!
//! - la migration 101 a posé `track_credits.source` et s'est inscrite ;
//! - son attribution unique, rejouée telle qu'écrite dans le script mais
//!   bornée aux pistes du banc, donne `roon`, `musicbrainz` ou NULL ;
//! - l'écriture MusicBrainz (`ecrire_credits_piste`) laisse la ligne
//!   `balises` et ne purge que ses lignes et les lignes NULL ;
//! - les deux filtres partagés des passes « à faire »
//!   (`source_des_credits::PISTE_SANS_CREDIT` et `CREDITS_D_UNE_PISTE`),
//!   exécutés TELS QUELS, gardent candidate une piste aux seules balises.
//!
//! Même doctrine que `pg_credits_4984.rs` : variable ABSENTE ⇒ saut annoncé ;
//! variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use tune_core::db::backend::{DbBackend, SqlValue, ToSqlValue};
use tune_core::metadata::credits_mb::LigneCredit;
use tune_core::metadata::credits_release::ecrire_credits_piste;
use tune_core::metadata::source_des_credits::{
    BALISES, CREDITS_D_UNE_PISTE, MUSICBRAINZ, PISTE_SANS_CREDIT, ROON,
};
use tune_server::state::AppState;

/// Suffixe propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide rien et on ne compte que nos lignes.
const MARQUE: &str = "credits-source-7765";

/// Le script de la jumelle PG, pour en rejouer l'attribution telle qu'écrite.
const SCRIPT_101: &str = include_str!("../../tune-core/migrations/postgres/101_credits_source.sql");

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
            "DELETE FROM track_metadata WHERE track_id IN \
             (SELECT id FROM tracks WHERE title LIKE '{MARQUE}%')"
        ),
        format!(
            "DELETE FROM track_credits WHERE track_id IN \
             (SELECT id FROM tracks WHERE title LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM tracks WHERE title LIKE '{MARQUE}%'"),
    ] {
        b.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

fn piste(b: &dyn DbBackend, suffixe: &str) -> i64 {
    let titre = format!("{MARQUE}-{suffixe}");
    b.execute_returning_id(
        "INSERT INTO tracks (title, musicbrainz_recording_id) VALUES (?, ?)",
        &[&titre as &dyn ToSqlValue, &titre as &dyn ToSqlValue],
    )
    .unwrap_or_else(|e| panic!("piste {titre} : {e}"))
}

fn credit(b: &dyn DbBackend, track_id: i64, nom: &str, mbid: Option<&str>, source: Option<&str>) {
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, position, artist_mbid, source) \
         VALUES (?, ?, 'performer', 0, ?, ?)",
        &[
            &track_id as &dyn ToSqlValue,
            &nom as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
            &source as &dyn ToSqlValue,
        ],
    )
    .unwrap_or_else(|e| panic!("crédit {nom} : {e}"));
}

fn sources(b: &dyn DbBackend, track_id: i64) -> Vec<(String, Option<String>)> {
    b.query_many(
        "SELECT artist_name, source FROM track_credits WHERE track_id = ? ORDER BY id",
        &[&track_id as &dyn ToSqlValue],
    )
    .expect("lecture des crédits")
    .iter()
    .map(|r| {
        (
            r.first().and_then(|v| v.as_string()).unwrap_or_default(),
            r.get(1).and_then(|v| v.as_string()),
        )
    })
    .collect()
}

fn entier(b: &dyn DbBackend, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    match b
        .query_one(sql, params)
        .unwrap_or_else(|e| panic!("« {sql} » : {e}"))
        .expect("une ligne")[0]
    {
        SqlValue::Int(n) => n,
        ref autre => panic!("« {sql} » n'a pas rendu un entier : {autre:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_7765_la_source_des_credits_attribuee_ecrite_et_filtree() {
    let Some(url) = url_pg() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    let b: &dyn DbBackend = state.backend.as_ref();
    menage(b);

    // La migration 101 est jouée : colonne posée, version inscrite.
    assert_eq!(
        entier(
            b,
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_name = 'track_credits' AND column_name = 'source'",
            &[],
        ),
        1,
        "`track_credits.source` manque sur PostgreSQL (migration 101)"
    );
    assert_eq!(
        entier(
            b,
            "SELECT COUNT(*) FROM schema_version WHERE version = 101 AND name = 'credits_source'",
            &[],
        ),
        1,
        "schema_version n'inscrit pas la 101 `credits_source`"
    );

    // Attribution unique : l'UPDATE du script, borné aux pistes du banc.
    let roon = piste(b, "roon");
    let mb = piste(b, "mb");
    let inconnue = piste(b, "inconnue");
    b.execute(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'credits_source', 'roon')",
        &[&roon as &dyn ToSqlValue],
    )
    .expect("marque Roon");
    credit(b, roon, "David Eugene Edwards", None, None);
    credit(
        b,
        mb,
        "Neil Young",
        Some("75167b8b-44e4-407b-9d35-effe87b223cf"),
        None,
    );
    credit(b, inconnue, "Inconnu", Some(""), None);
    let debut = SCRIPT_101
        .find("UPDATE track_credits")
        .expect("UPDATE dans la 101");
    let fin = debut
        + SCRIPT_101[debut..]
            .find("WHERE source IS NULL;")
            .expect("borne de l'UPDATE");
    let attribution = format!(
        "{} WHERE source IS NULL AND track_id IN (SELECT id FROM tracks WHERE title LIKE '{MARQUE}%')",
        &SCRIPT_101[debut..fin]
    );
    b.execute(&attribution, &[])
        .unwrap_or_else(|e| panic!("attribution de la 101 sur PostgreSQL : {e}\n{attribution}"));
    assert_eq!(sources(b, roon)[0].1.as_deref(), Some(ROON), "piste Roon");
    assert_eq!(
        sources(b, mb)[0].1.as_deref(),
        Some(MUSICBRAINZ),
        "piste au MBID d'artiste"
    );
    assert_eq!(sources(b, inconnue)[0].1, None, "piste sans origine");

    // L'écriture MusicBrainz laisse la ligne de balises.
    let balises = piste(b, "balises");
    credit(b, balises, "Miles Davis", None, Some(BALISES));
    credit(b, balises, "Ancienne ligne", None, None);
    for passe in 1..=2 {
        let lignes = [LigneCredit {
            artist_name: "Paul Chambers".into(),
            role: "performer".into(),
            instrument: Some("bass".into()),
            artist_mbid: None,
        }];
        assert_eq!(
            ecrire_credits_piste(&state.backend, balises, MUSICBRAINZ, &lignes),
            1,
            "passe {passe} : l'écriture MusicBrainz a échoué sur PostgreSQL"
        );
    }
    let lus = sources(b, balises);
    assert_eq!(
        lus,
        vec![
            ("Miles Davis".to_string(), Some(BALISES.to_string())),
            ("Paul Chambers".to_string(), Some(MUSICBRAINZ.to_string())),
        ],
        "piste {balises} : la réécriture MusicBrainz doit garder la ligne `balises`, \
         remplacer la ligne NULL et ne pas doubler ses lignes"
    );

    // Les filtres partagés des passes « à faire », exécutés tels quels.
    let seules_balises = piste(b, "seules-balises");
    credit(b, seules_balises, "Bill Evans", None, Some(BALISES));
    let candidates: Vec<i64> = b
        .query_many(
            &format!(
                "SELECT t.id FROM tracks t WHERE t.title LIKE '{MARQUE}%' AND {PISTE_SANS_CREDIT} \
                 ORDER BY t.id"
            ),
            &[],
        )
        .expect("filtre PISTE_SANS_CREDIT sur PostgreSQL")
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(
        candidates,
        vec![seules_balises],
        "seule la piste aux seuls crédits de balises reste à faire sur PostgreSQL"
    );
    assert_eq!(
        entier(
            b,
            CREDITS_D_UNE_PISTE,
            &[&seules_balises as &dyn ToSqlValue]
        ),
        0,
        "CREDITS_D_UNE_PISTE compte les lignes `balises` sur PostgreSQL"
    );
    assert_eq!(
        entier(b, CREDITS_D_UNE_PISTE, &[&balises as &dyn ToSqlValue]),
        1,
        "CREDITS_D_UNE_PISTE doit compter la ligne MusicBrainz"
    );

    menage(b);
}
