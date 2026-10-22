//! #6516 — la migration 126 / PG 090 pose `radio_stations.external_id`, le
//! lien d'une station avec sa fiche de l'annuaire radio-metadata.fr.

use super::*;
use crate::db::radio_repo::{RadioRepo, RadioStation};

fn colonne(db: &SqliteDb) -> Option<(String, bool)> {
    let conn = db.connection().lock().unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(radio_stations)").unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap())
        .find(|(nom, _, _)| nom == "external_id")
        .map(|(_, ty, non_nul)| (ty, non_nul != 0))
}

fn index_pose(db: &SqliteDb) -> bool {
    let conn = db.connection().lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_radio_stations_external_id'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

#[test]
fn la_migration_126_pose_le_lien_des_radios_sur_une_base_neuve_et_montee() {
    let attendu = Some(("TEXT".to_string(), false));

    let neuve = SqliteDb::open_in_memory().unwrap();
    neuve.init_schema().unwrap();
    run_migrations(&neuve).unwrap();
    assert_eq!(
        colonne(&neuve),
        attendu,
        "base neuve : external_id TEXT nullable"
    );
    assert!(index_pose(&neuve), "base neuve : index absent");

    // Base existante, montée en 125, avec ses stations semées et une à soi.
    let montee = SqliteDb::open_in_memory().unwrap();
    montee.init_schema().unwrap();
    run_migrations(&montee).unwrap();
    montee
        .execute_batch(
            "DROP INDEX IF EXISTS idx_radio_stations_external_id;
             ALTER TABLE radio_stations DROP COLUMN external_id;
             DELETE FROM _migrations WHERE version >= 126;
             INSERT INTO radio_stations (name, url) VALUES ('À moi', 'https://exemple.invalid/a');",
        )
        .unwrap();
    assert_eq!(colonne(&montee), None, "préparation : colonne retirée");
    assert_eq!(current_version(&montee).unwrap(), 125);
    let avant = RadioRepo::new(montee.clone()).count().unwrap();
    run_migrations(&montee).unwrap();
    assert_eq!(current_version(&montee).unwrap(), latest_version());
    assert!(latest_version() >= 126);
    assert_eq!(
        colonne(&montee),
        attendu,
        "base montée : external_id TEXT nullable"
    );
    assert!(index_pose(&montee), "base montée : index absent");
    let repo = RadioRepo::new(montee.clone());
    assert_eq!(repo.count().unwrap(), avant, "aucune station perdue");
    assert!(
        repo.liens_annuaire().unwrap().is_empty(),
        "aucun lien inventé"
    );

    // Rejouer est sans effet.
    run_migrations(&montee).unwrap();
    assert_eq!(colonne(&montee), attendu);
}

#[test]
fn le_lien_se_pose_se_relit_et_se_retire() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let repo = RadioRepo::new(db);
    let id = repo
        .create(&RadioStation {
            id: None,
            name: "FIP".into(),
            url: "https://icecast.radiofrance.fr/fip-hifi.aac?essai".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    repo.lier_a_l_annuaire(id, Some("S000973")).unwrap();
    assert_eq!(
        repo.liens_annuaire().unwrap().get(&id).map(String::as_str),
        Some("S000973")
    );
    // La station elle-même ne change pas : l'adresse reste la sienne.
    assert_eq!(
        repo.get(id).unwrap().unwrap().url,
        "https://icecast.radiofrance.fr/fip-hifi.aac?essai"
    );
    assert_eq!(repo.lien_annuaire(id).unwrap().as_deref(), Some("S000973"));
    repo.lier_a_l_annuaire(id, None).unwrap();
    assert_eq!(repo.lien_annuaire(id).unwrap(), None);
    assert!(!repo.liens_annuaire().unwrap().contains_key(&id));
}

#[test]
fn la_jumelle_pg_090_est_posee_partout() {
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fichier = "090_radio_stations_external_id.sql";
    let sql_pg = std::fs::read_to_string(racine.join("migrations/postgres").join(fichier)).unwrap();
    assert!(sql_pg.contains("VALUES (90, 'radio_stations_external_id')"));
    assert!(sql_pg.contains("ADD COLUMN IF NOT EXISTS external_id TEXT"));
    assert!(sql_pg.contains("idx_radio_stations_external_id"));
    assert!(include_str!("migrations.rs").contains(fichier));
    assert!(
        include_str!("pg_migrate.rs")
            .contains("ALTER TABLE radio_stations ADD COLUMN IF NOT EXISTS external_id TEXT")
    );
    assert!(
        include_str!("postgres.rs")
            .contains("ALTER TABLE radio_stations ADD COLUMN IF NOT EXISTS external_id TEXT")
    );
}
