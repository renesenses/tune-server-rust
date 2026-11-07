//! #7983 — la trace des tentatives d'enrichissement, sur une vraie base
//! SQLite migrée.
use super::*;
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    backend
        .execute_batch(
            "INSERT INTO tracks (id, title, file_path, source, file_mtime) \
               VALUES (1, 'Un', '/m/1.flac', 'local', 1700000000.5);",
        )
        .unwrap();
    backend
}

fn ligne(db: &Arc<dyn DbBackend>) -> (String, i64, i64) {
    let r = db
        .query_one_strong(
            "SELECT issue, echecs, reessai_apres FROM enrich_tentatives WHERE track_id = 1",
            &[],
        )
        .unwrap()
        .expect("la tentative est notée");
    (
        r[0].as_string().unwrap(),
        r[1].as_i64().unwrap(),
        r[2].as_i64().unwrap(),
    )
}

#[test]
fn le_delai_double_a_chaque_echec_et_se_plafonne() {
    assert_eq!(delai_avant_reessai(1), 3600);
    assert_eq!(delai_avant_reessai(2), 7200);
    assert_eq!(delai_avant_reessai(3), 14_400);
    assert_eq!(delai_avant_reessai(100), DELAI_MAX_S);
    assert_eq!(
        delai_avant_reessai(0),
        3600,
        "un compte nul vaut un premier échec"
    );
}

#[test]
fn un_echec_temporaire_repousse_la_tentative_et_compte_les_echecs() {
    let db = base();
    let t0 = 1_000_000;
    assert_eq!(
        noter(&db, 1, Issue::EchecTemporaire, t0).unwrap(),
        t0 + 3600
    );
    assert_eq!(ligne(&db), ("echec_temporaire".into(), 1, t0 + 3600));
    let t1 = t0 + 4000;
    noter(&db, 1, Issue::EchecTemporaire, t1).unwrap();
    assert_eq!(ligne(&db), ("echec_temporaire".into(), 2, t1 + 7200));
    assert_eq!(compter_a_retenter(&db, t1).unwrap(), 1);
    assert_eq!(compter_a_retenter(&db, t1 + 7200).unwrap(), 0);
    noter(&db, 1, Issue::Fait, t1 + 8000).unwrap();
    assert_eq!(
        ligne(&db),
        ("fait".into(), 0, 0),
        "un succès remet le compte à zéro"
    );
}

#[test]
fn la_marque_garde_la_date_du_fichier_vu() {
    let db = base();
    noter(&db, 1, Issue::Introuvable, 10).unwrap();
    let vu = db
        .query_one_strong(
            "SELECT mtime_vu FROM enrich_tentatives WHERE track_id = 1",
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_string();
    let fichier = db
        .query_one_strong(
            "SELECT CAST(file_mtime AS TEXT) FROM tracks WHERE id = 1",
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_string();
    assert!(vu.is_some());
    assert_eq!(vu, fichier);
}

#[test]
fn une_piste_inconnue_n_est_pas_notee() {
    let db = base();
    noter(&db, 99, Issue::Fait, 10).unwrap();
    let n = db
        .query_one_strong("SELECT COUNT(*) FROM enrich_tentatives", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64();
    assert_eq!(n, Some(0));
}
