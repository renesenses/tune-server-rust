//! #7974 — témoins du bail d'écoute : plages, budgets, remise à zéro
//! quotidienne, stockage SQLite et PostgreSQL.

use super::*;
use crate::db::politique_parentale::{PlageHoraire, poser};
use chrono::NaiveDate;

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;

fn a(jour: (i32, u32, u32), h: u32, m: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(jour.0, jour.1, jour.2)
        .unwrap()
        .and_hms_opt(h, m, 0)
        .unwrap()
}

/// Lundi 2 novembre 2026.
const LUNDI: (i32, u32, u32) = (2026, 11, 2);
/// Samedi 7 novembre 2026.
const SAMEDI: (i32, u32, u32) = (2026, 11, 7);

fn semaine_de_17_a_19h() -> PolitiqueParentale {
    PolitiqueParentale {
        plages_horaires: vec![PlageHoraire {
            jours: vec![1, 2, 3, 4, 5],
            debut: "17:00".into(),
            fin: "19:00".into(),
        }],
        ..Default::default()
    }
}

#[test]
fn bail_7974_hors_plage_refuse() {
    let p = semaine_de_17_a_19h();
    assert_eq!(evaluer(&p, a(LUNDI, 16, 59), 0, 0), Err(Refus::HorsPlage));
    assert_eq!(evaluer(&p, a(LUNDI, 19, 0), 0, 0), Err(Refus::HorsPlage));
    assert_eq!(
        evaluer(&p, a(SAMEDI, 18, 0), 0, 0),
        Err(Refus::HorsPlage),
        "le samedi n'est pas dans la plage"
    );
    assert_eq!(
        evaluer(&p, a(LUNDI, 18, 30), 0, 0),
        Ok(Bail {
            reste_s: Some(30 * 60)
        }),
        "dans la plage, le bail dit le temps qui reste avant sa fin"
    );
}

#[test]
fn bail_7974_budget_epuise_refuse() {
    let p = PolitiqueParentale {
        budget_quotidien_minutes: Some(60),
        budget_hebdomadaire_minutes: Some(180),
        ..Default::default()
    };
    assert_eq!(
        evaluer(&p, a(LUNDI, 10, 0), 59 * 60, 59 * 60),
        Ok(Bail { reste_s: Some(60) })
    );
    assert_eq!(
        evaluer(&p, a(LUNDI, 10, 0), 3600, 3600),
        Err(Refus::BudgetQuotidienEpuise)
    );
    assert_eq!(
        evaluer(&p, a(SAMEDI, 10, 0), 0, 180 * 60),
        Err(Refus::BudgetHebdomadaireEpuise)
    );
    let zero = PolitiqueParentale {
        budget_quotidien_minutes: Some(0),
        ..Default::default()
    };
    assert_eq!(
        evaluer(&zero, a(LUNDI, 10, 0), 0, 0),
        Err(Refus::BudgetQuotidienEpuise),
        "un budget de zéro minute ne laisse rien jouer"
    );
}

#[test]
fn bail_7974_plage_et_budget_le_plus_court_l_emporte() {
    let mut p = semaine_de_17_a_19h();
    p.budget_quotidien_minutes = Some(30);
    assert_eq!(
        evaluer(&p, a(LUNDI, 17, 0), 20 * 60, 20 * 60),
        Ok(Bail {
            reste_s: Some(10 * 60)
        })
    );
}

#[test]
fn bail_7974_politique_sans_limite_de_lecture() {
    let p = PolitiqueParentale {
        radios_autorisees: Some(vec![]),
        ..Default::default()
    };
    assert!(!limite_la_lecture(&p));
    assert_eq!(
        evaluer(&p, a(LUNDI, 3, 0), 1_000_000, 1_000_000),
        Ok(Bail { reste_s: None })
    );
}

#[test]
fn bail_7974_sentinelle_relue() {
    for r in [
        Refus::HorsPlage,
        Refus::BudgetQuotidienEpuise,
        Refus::BudgetHebdomadaireEpuise,
        Refus::PolitiqueIllisible,
    ] {
        assert_eq!(Refus::depuis_sentinelle(&r.sentinelle()), Some(r));
    }
    assert_eq!(Refus::depuis_sentinelle("free_zone_cap:1:2"), None);
}

fn sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE profiles (id INTEGER PRIMARY KEY, username TEXT, is_admin INTEGER DEFAULT 0)",
    )
    .unwrap();
    db.execute_batch(crate::db::politique_parentale::SQL_TABLE)
        .unwrap();
    db.execute_batch(SQL_TABLE).unwrap();
    Arc::new(db)
}

/// Le même scénario sur les deux moteurs.
fn verifier(db: Arc<dyn DbBackend>) {
    let lundi = NaiveDate::from_ymd_opt(LUNDI.0, LUNDI.1, LUNDI.2).unwrap();
    let mardi = lundi.succ_opt().unwrap();
    let lundi_suivant = lundi + chrono::Duration::days(7);

    // Sans politique : aucun bail demandé, même à 3 h du matin.
    assert_eq!(demander(&db, ENFANT, a(LUNDI, 3, 0)), Ok(None));

    let p = PolitiqueParentale {
        budget_quotidien_minutes: Some(60),
        budget_hebdomadaire_minutes: Some(100),
        ..Default::default()
    };
    poser(&db, ENFANT, ADMIN, 0, &p).unwrap();
    assert_eq!(
        demander(&db, ENFANT, a(LUNDI, 10, 0)),
        Ok(Some(Bail {
            reste_s: Some(3600)
        }))
    );

    crediter(&db, ENFANT, lundi, 1800).unwrap();
    crediter(&db, ENFANT, lundi, 1800).unwrap();
    crediter(&db, ADMIN, lundi, 9999).unwrap();
    assert_eq!(
        ecoute_du_jour_et_de_la_semaine(&db, ENFANT, lundi).unwrap(),
        (3600, 3600),
        "les écritures s'additionnent, par profil"
    );
    assert_eq!(
        demander(&db, ENFANT, a(LUNDI, 20, 0)),
        Err(Refus::BudgetQuotidienEpuise)
    );

    // Remise à zéro quotidienne : le mardi repart de zéro pour le jour, la
    // semaine garde le lundi.
    assert_eq!(
        ecoute_du_jour_et_de_la_semaine(&db, ENFANT, mardi).unwrap(),
        (0, 3600)
    );
    assert_eq!(
        demander(&db, ENFANT, a((2026, 11, 3), 8, 0)),
        Ok(Some(Bail {
            reste_s: Some(40 * 60)
        })),
        "mardi : le budget du jour est rendu, borné par celui de la semaine"
    );
    crediter(&db, ENFANT, mardi, 40 * 60).unwrap();
    assert_eq!(
        demander(&db, ENFANT, a((2026, 11, 3), 9, 0)),
        Err(Refus::BudgetHebdomadaireEpuise)
    );
    // La semaine suivante repart de zéro.
    assert_eq!(
        ecoute_du_jour_et_de_la_semaine(&db, ENFANT, lundi_suivant).unwrap(),
        (0, 0)
    );

    // Une politique illisible refuse.
    db.execute(
        "UPDATE politiques_parentales SET politique_json = '{illisible' WHERE profile_id = ?",
        &[&ENFANT as &dyn ToSqlValue],
    )
    .unwrap();
    assert_eq!(
        demander(&db, ENFANT, a(LUNDI, 10, 0)),
        Err(Refus::PolitiqueIllisible)
    );
}

#[test]
fn bail_7974_sqlite() {
    verifier(sqlite());
}

/// La vraie passe de migration SQLite pose la table, et la rejouer ne casse
/// rien.
#[test]
fn bail_7974_table_posee_par_les_migrations() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let jour = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
    crediter(&db, ENFANT, jour, 12).unwrap();
    assert_eq!(ecoute_entre(&db, ENFANT, jour, jour).unwrap(), 12);
}

/// Table de politiques absente : la lecture reste libre.
#[test]
fn bail_7974_table_absente_laisse_jouer() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    assert_eq!(demander(&db, ENFANT, a(LUNDI, 3, 0)), Ok(None));
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7974_bail_d_ecoute_postgres() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    // Une seule connexion : les tables temporaires masquent celles de la base
    // de test et disparaissent avec elle.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TEMPORARY TABLE profiles (id BIGINT PRIMARY KEY, username TEXT, is_admin SMALLINT DEFAULT 0)")
        .execute(&pool)
        .await
        .unwrap();
    for ddl in [crate::db::politique_parentale::SQL_TABLE, SQL_TABLE] {
        let ddl = ddl.replace("CREATE TABLE IF NOT EXISTS", "CREATE TEMPORARY TABLE");
        // `AssertSqlSafe` : `ddl` vient des constantes `SQL_TABLE`, aucune entrée.
        sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
            .execute(&pool)
            .await
            .unwrap();
    }
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    verifier(db);
    pool.close().await;
}
