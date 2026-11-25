//! Retour de Rémi sur la rc13 (GO de Bertrand) — une installation NEUVE doit
//! montrer tout de suite ce que Tune sait faire, sans rien imposer.
//!
//! 1. Aucune radio d'office : les semis de « Mes radios » (migrations 33, 90
//!    et 98) sont sautés sur une base qui naît, et notés quand même.
//! 2. Une base existante n'est jamais touchée : ses stations restent, et une
//!    base en retard reçoit encore les semis qu'elle n'a pas joués.
//! 3. Les paroles en ligne (LRCLIB) sont activées tant que personne ne les a
//!    décochées ; un refus posé reste un refus.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::migrations::{SEMIS_DE_RADIOS_SQLITE, run_migrations};
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::library::lyrics_pass::{SETTING_LRCLIB_ENABLED, lrclib_consent_given};

fn base_neuve() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    db
}

fn compter(db: &SqliteDb, sql: &str) -> i64 {
    let conn = db.connection().lock().unwrap();
    conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap()
}

#[test]
fn une_installation_neuve_n_a_aucune_radio_dans_mes_radios() {
    let db = base_neuve();
    assert_eq!(
        compter(&db, "SELECT COUNT(*) FROM radio_stations"),
        0,
        "une base neuve ne doit recevoir AUCUNE station d'office"
    );
}

#[test]
fn les_semis_sautes_sont_quand_meme_notes() {
    let db = base_neuve();
    for v in SEMIS_DE_RADIOS_SQLITE {
        assert_eq!(
            compter(
                &db,
                &format!("SELECT COUNT(*) FROM _migrations WHERE version = {v}")
            ),
            1,
            "la migration {v} doit être notée, sinon elle serait rejouée plus tard"
        );
    }
}

#[test]
fn un_redemarrage_ne_seme_rien_et_garde_les_stations_ajoutees() {
    let db = base_neuve();
    db.execute_batch(
        "INSERT INTO radio_stations (name, url) VALUES ('Ma station', 'https://exemple.invalid/flux')",
    )
    .unwrap();
    run_migrations(&db).unwrap();
    assert_eq!(compter(&db, "SELECT COUNT(*) FROM radio_stations"), 1);
}

/// Une base EXISTANTE (des migrations déjà notées) qui n'a pas encore joué un
/// semis le reçoit toujours : seule la base qui naît en est privée.
#[test]
fn une_base_existante_en_retard_recoit_encore_ses_semis() {
    let db = base_neuve();
    db.execute_batch("DELETE FROM _migrations WHERE version >= 90")
        .unwrap();
    run_migrations(&db).unwrap();
    assert!(
        compter(&db, "SELECT COUNT(*) FROM radio_stations") > 0,
        "une base existante qui n'avait pas joué la 90 doit recevoir son semis, comme avant"
    );
}

#[test]
fn les_paroles_en_ligne_sont_activees_tant_que_personne_ne_les_a_decochees() {
    let db: Arc<dyn DbBackend> = Arc::new(base_neuve());
    assert!(
        lrclib_consent_given(&db),
        "réglage jamais posé : LRCLIB activé d'office"
    );
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages.set(SETTING_LRCLIB_ENABLED, "false").unwrap();
    assert!(
        !lrclib_consent_given(&db),
        "une case décochée reste décochée"
    );
    // Un redémarrage ne réécrit pas le choix.
    reglages.set(SETTING_LRCLIB_ENABLED, "true").unwrap();
    assert!(lrclib_consent_given(&db));
}
