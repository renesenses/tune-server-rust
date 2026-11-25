//! Retour de Rémi sur la rc13 (GO de Bertrand) — une installation NEUVE doit
//! montrer tout de suite ce que Tune sait faire, sans rien imposer.
//!
//! 1. Aucune radio d'office : les semis de « Mes radios » (migrations 33, 90
//!    et 98) sont sautés sur une base qui naît, et notés quand même.
//! 2. Une base existante n'est jamais touchée : ses stations restent, et une
//!    base en retard reçoit encore les semis qu'elle n'a pas joués.
//! 3. Les paroles en ligne (LRCLIB) sont POSÉES à « activé » à la naissance
//!    de la base ; une base existante sans réglage reste désactivée ; un
//!    choix explicite est respecté.

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
fn une_base_neuve_nait_avec_les_paroles_en_ligne_activees() {
    let db: Arc<dyn DbBackend> = Arc::new(base_neuve());
    assert_eq!(
        SettingsRepo::with_backend(db.clone())
            .get(SETTING_LRCLIB_ENABLED)
            .unwrap()
            .as_deref(),
        Some("true"),
        "le réglage est POSÉ à la naissance de la base"
    );
    assert!(lrclib_consent_given(&db));
}

/// Une base EXISTANTE qui n'a jamais touché la case : rien ne change, aucun
/// envoi nouveau à lrclib.net (décision de Bertrand, rc14).
#[test]
fn une_base_existante_sans_reglage_reste_desactivee() {
    let db = base_neuve();
    db.execute_batch("DELETE FROM settings WHERE key = 'lyrics_lrclib_enabled'")
        .unwrap();
    // Redémarrage : la base n'est plus neuve, rien n'est posé.
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    assert_eq!(
        SettingsRepo::with_backend(db.clone())
            .get(SETTING_LRCLIB_ENABLED)
            .unwrap(),
        None
    );
    assert!(
        !lrclib_consent_given(&db),
        "réglage absent sur une base existante : désactivé"
    );
}

#[test]
fn un_reglage_explicite_est_respecte() {
    let db = base_neuve();
    db.execute_batch("UPDATE settings SET value = 'false' WHERE key = 'lyrics_lrclib_enabled'")
        .unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    assert!(
        !lrclib_consent_given(&db),
        "une case décochée reste décochée"
    );
    SettingsRepo::with_backend(db.clone())
        .set(SETTING_LRCLIB_ENABLED, "true")
        .unwrap();
    assert!(lrclib_consent_given(&db));
}

#[test]
fn la_lecture_pure_du_reglage() {
    use crate::library::lyrics_pass::lrclib_consentement_depuis;
    assert!(!lrclib_consentement_depuis(None));
    assert!(!lrclib_consentement_depuis(Some("false")));
    assert!(lrclib_consentement_depuis(Some("true")));
}
