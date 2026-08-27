//! Validation de la migration 73 sur une COPIE d'une base réelle.
//!
//! Ignoré par défaut : ne tourne que si `TUNE_REAL_DB` désigne une copie.
//!   TUNE_REAL_DB=/chemin/copie.db cargo test -p tune-core --test integration_contracts migration_on_real_db:: -- --nocapture

use tune_core::db::sqlite::SqliteDb;

#[test]
fn merge_scattered_on_a_real_database() {
    let Ok(path) = std::env::var("TUNE_REAL_DB") else {
        eprintln!("TUNE_REAL_DB absent — test ignoré");
        return;
    };
    let db = SqliteDb::open(&path).unwrap();

    let count = |sql: &str| -> i64 {
        let conn = db.connection().lock().unwrap();
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    };
    let avant_albums = count("SELECT COUNT(*) FROM albums");
    let avant_pistes = count("SELECT COUNT(*) FROM tracks");
    let avant_orphelines = count("SELECT COUNT(*) FROM tracks WHERE album_id IS NULL");

    let t0 = std::time::Instant::now();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let duree = t0.elapsed();

    let apres_albums = count("SELECT COUNT(*) FROM albums");
    let apres_pistes = count("SELECT COUNT(*) FROM tracks");
    let apres_orphelines = count("SELECT COUNT(*) FROM tracks WHERE album_id IS NULL");
    let vides = count(
        "SELECT COUNT(*) FROM albums a WHERE NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id)",
    );

    println!("=== migration sur base réelle ({duree:?}) ===");
    println!(
        "albums   : {avant_albums} -> {apres_albums}  (fusionnés : {})",
        avant_albums - apres_albums
    );
    println!("pistes   : {avant_pistes} -> {apres_pistes}");
    println!("orphelines : {avant_orphelines} -> {apres_orphelines}");
    println!("albums sans piste après migration : {vides}");

    assert_eq!(
        apres_pistes, avant_pistes,
        "AUCUNE piste ne doit disparaître"
    );
    assert_eq!(
        apres_orphelines, avant_orphelines,
        "aucune piste ne doit perdre son album"
    );
}
