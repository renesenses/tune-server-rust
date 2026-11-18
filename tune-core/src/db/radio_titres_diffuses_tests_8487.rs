//! #8487 — témoins de la table des titres diffusés : le bruit est écarté, un
//! titre répété n'est noté qu'une fois, un titre ne se range que sous la
//! station en cours de sa zone, et les statistiques n'en voient rien.

use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::history_repo::HistoryRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;

const FLUX: &str = "http://127.0.0.1:9/bbc6-8487.mp3";
const STATION: &str = "BBC Radio 6 Music";

fn base() -> (Arc<dyn DbBackend>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Parents", Some("dlna"), Some("uuid:8487"))
        .unwrap();
    (db, zone)
}

/// La ligne que `enregistrer_ecoute_radio` (#7965) écrit au lancement.
fn ligne_de_station(db: &Arc<dyn DbBackend>, zone: i64, flux: &str) -> i64 {
    db.execute_returning_id(
        "INSERT INTO listen_history (title, source, source_id, zone_id, context_type, \
         context_id, context_source, context_title) \
         VALUES (?, 'radio', ?, ?, 'radio', '12', 'radio', ?)",
        &[&STATION as &dyn ToSqlValue, &flux, &zone, &STATION],
    )
    .unwrap()
}

fn titres(db: &Arc<dyn DbBackend>) -> i64 {
    db.query_one("SELECT COUNT(*) FROM radio_titres_diffuses", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

#[test]
fn le_bruit_d_antenne_n_est_pas_un_titre_8487() {
    for (artiste, titre) in [
        (None, ""),
        (None, "   "),
        (None, "-"),
        (None, "BBC Radio 6 Music"),
        (Some("BBC Radio 6 Music"), "Now Playing"),
        (None, "bbc radio 6 music - live"),
        (None, "https://www.bbc.co.uk/6music"),
        (None, "Unknown"),
        (Some("Unknown"), "Unknown"),
        (None, "Pub"),
        (None, "Station Jingle"),
        (None, "Jingle 6 Music"),
        (None, "Advertisement"),
        (Some("Pub"), "Spot Renault"),
    ] {
        assert!(
            !titre_a_noter(STATION, artiste, titre),
            "« {artiste:?} / {titre} » n'est pas un morceau"
        );
    }
    for (artiste, titre) in [
        (Some("King Gizzard & the Lizard Wizard"), "Rattlesnake"),
        (Some("Bobby Helms"), "Jingle Bell Rock"),
        (None, "Miles Davis - So What"),
        (Some("Nova"), "Supernova"),
    ] {
        assert!(
            titre_a_noter(STATION, artiste, titre),
            "« {artiste:?} / {titre} » est un morceau"
        );
    }
}

#[test]
fn un_titre_diffuse_est_note_sous_sa_station_une_seule_fois_8487() {
    let (db, zone) = base();
    let station = ligne_de_station(&db, zone, FLUX);
    let gizz = Some("King Gizzard & the Lizard Wizard");

    let n = |a: Option<&str>, t: &str, ms: i64| {
        noter(&db, zone, FLUX, STATION, a, t, None, ms).unwrap()
    };
    assert_eq!(n(gizz, "Rattlesnake", 1_000), Notation::Ecrit);
    // Le sondeur relit le flux : même titre, casse et espaces près.
    assert_eq!(
        n(
            Some("king gizzard  & the lizard wizard"),
            " Rattlesnake",
            16_000
        ),
        Notation::DejaNote
    );
    // Un jingle entre deux lectures du même titre n'en fait pas deux.
    assert_eq!(n(None, "Station Jingle", 31_000), Notation::PasUnTitre);
    assert_eq!(n(gizz, "Rattlesnake", 46_000), Notation::DejaNote);
    assert_eq!(
        n(Some("Fontaines D.C."), "Starburster", 200_000),
        Notation::Ecrit
    );
    assert_eq!(titres(&db), 2, "deux morceaux, deux lignes");

    let rendus = pour_ecoutes(&db, &[station]).unwrap();
    let sous_la_station = &rendus[&station];
    assert_eq!(sous_la_station.len(), 2);
    assert_eq!(
        sous_la_station[0].titre, "Starburster",
        "le plus récent d'abord"
    );
    assert_eq!(sous_la_station[1].titre, "Rattlesnake");
    assert_eq!(sous_la_station[1].artiste.as_deref(), gizz);
    assert_eq!(sous_la_station[1].diffuse_a, "1970-01-01T00:00:01Z");
    assert_eq!(sous_la_station[1].zone_id, Some(zone));
}

#[test]
fn sans_station_en_cours_rien_n_est_note_8487() {
    let (db, zone) = base();
    // Aucune ligne : zone navigateur dont l'écoute n'est pas encore confirmée.
    assert_eq!(
        noter(&db, zone, FLUX, STATION, Some("A"), "B", None, 1).unwrap(),
        Notation::PasDeStation
    );
    // La station d'un AUTRE flux.
    ligne_de_station(&db, zone, "http://127.0.0.1:9/autre.mp3");
    assert_eq!(
        noter(&db, zone, FLUX, STATION, Some("A"), "B", None, 2).unwrap(),
        Notation::PasDeStation
    );
    // La station, puis un album lancé sur la même zone : la session est finie.
    ligne_de_station(&db, zone, FLUX);
    db.execute(
        "INSERT INTO listen_history (title, source, zone_id) VALUES ('Kind of Blue', 'local', ?)",
        &[&zone as &dyn ToSqlValue],
    )
    .unwrap();
    assert_eq!(
        noter(&db, zone, FLUX, STATION, Some("A"), "B", None, 3).unwrap(),
        Notation::PasDeStation
    );
    assert_eq!(titres(&db), 0);
}

#[test]
fn les_statistiques_ne_voient_pas_les_titres_diffuses_8487() {
    let (db, zone) = base();
    ligne_de_station(&db, zone, FLUX);
    let repo = HistoryRepo::with_backend(db.clone());
    let avant = (
        repo.count().unwrap(),
        repo.recent_paginated(50, 0).unwrap().1,
        repo.top_tracks(20).unwrap(),
        repo.top_artists(20).unwrap(),
    );
    for (i, (a, t)) in [
        ("Wet Leg", "Chaise Longue"),
        ("Idles", "Grace"),
        ("Bicep", "Glue"),
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(
            noter(&db, zone, FLUX, STATION, Some(a), t, None, i as i64 * 1000).unwrap(),
            Notation::Ecrit
        );
    }
    let apres = (
        repo.count().unwrap(),
        repo.recent_paginated(50, 0).unwrap().1,
        repo.top_tracks(20).unwrap(),
        repo.top_artists(20).unwrap(),
    );
    assert_eq!(
        avant, apres,
        "trois titres diffusés ne changent aucune statistique"
    );
    assert_eq!(titres(&db), 3);

    repo.clear().unwrap();
    assert_eq!(
        titres(&db),
        0,
        "effacer l'historique emporte les titres diffusés"
    );
}

/// La même écriture et la même lecture sur un vrai PostgreSQL : la table
/// (`ENSURE_TABLES`), la jointure à `listen_history`, les marqueurs `?`
/// traduits, l'insertion sans colonne `id` (#3256). Sautée sans
/// `TUNE_TEST_PG_URL` ; une base posée mais injoignable fait TOMBER le test.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_titres_diffuses_notes_et_relus_8487() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL absente — épreuve PostgreSQL 8487 sautée");
        return;
    };
    // Comme au premier démarrage : scripts numérotés, puis `ensure_schema`,
    // qui crée la table des titres diffusés (`ENSURE_TABLES`) et les colonnes
    // complémentaires de `listen_history` (`source_id`…).
    let postgres = crate::db::postgres::PostgresDb::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée mais injoignable ({url}) : {e}"));
    crate::db::migrations::run_pg_migrations(postgres.pool())
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    postgres.ensure_schema().await;
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(
        postgres.pool().clone(),
    ));
    let zone = db
        .execute_returning_id(
            "INSERT INTO zones (name) VALUES (?)",
            &[&"Parents 8487" as &dyn ToSqlValue],
        )
        .unwrap();
    let flux = format!("{FLUX}?pg={zone}");
    let station = ligne_de_station(&db, zone, &flux);
    let gizz = Some("King Gizzard & the Lizard Wizard");
    let n = |a: Option<&str>, t: &str, ms: i64| {
        noter(&db, zone, &flux, STATION, a, t, None, ms).unwrap()
    };
    assert_eq!(n(gizz, "Rattlesnake", 1_000), Notation::Ecrit);
    assert_eq!(n(gizz, "Rattlesnake", 16_000), Notation::DejaNote);
    assert_eq!(n(None, STATION, 20_000), Notation::PasUnTitre);
    assert_eq!(
        n(Some("Fontaines D.C."), "Starburster", 200_000),
        Notation::Ecrit
    );
    let rendus = pour_ecoutes(&db, &[station]).unwrap();
    let titres: Vec<&str> = rendus[&station].iter().map(|t| t.titre.as_str()).collect();
    assert_eq!(titres, vec!["Starburster", "Rattlesnake"]);
    assert_eq!(rendus[&station][1].diffuse_a, "1970-01-01T00:00:01Z");

    db.execute(
        "DELETE FROM radio_titres_diffuses WHERE ecoute_id = ?",
        &[&station as &dyn ToSqlValue],
    )
    .unwrap();
    db.execute(
        "DELETE FROM listen_history WHERE id = ?",
        &[&station as &dyn ToSqlValue],
    )
    .unwrap();
    db.execute(
        "DELETE FROM zones WHERE id = ?",
        &[&zone as &dyn ToSqlValue],
    )
    .unwrap();
}
