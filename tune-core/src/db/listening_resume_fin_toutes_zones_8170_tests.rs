//! #8170 — un épisode terminé sur une zone n'a plus de position sur AUCUNE
//! zone : écouté 20 min sur A puis jusqu'au bout sur B, la fiche du podcast
//! ne propose plus « reprendre à 20:00 ».

use super::*;

const URL: &str = "https://podcast.invalid/episode-8170.mp3";
const AUTRE: &str = "https://podcast.invalid/autre-8170.mp3";
const DUREE: i64 = 3_600_000;

fn verifier(db: Arc<dyn DbBackend>, a: i64, b: i64) {
    noter_position_d_episode(&db, a, URL, 1_200_000, DUREE).unwrap();
    noter_position_d_episode(&db, a, AUTRE, 300_000, DUREE).unwrap();
    noter_position_d_episode(&db, b, URL, 1_800_000, DUREE).unwrap();
    assert_eq!(positions_des_episodes(&db, Some(b)).unwrap().len(), 2);

    // Fin de l'épisode sur B, par le chemin du sondeur.
    podcast_position_avec_duree(&db, b, URL, DUREE - 10_000, DUREE).unwrap();

    for zone in [Some(a), Some(b), None] {
        let restantes = positions_des_episodes(&db, zone).unwrap();
        assert!(
            restantes.iter().all(|p| p.audio_url != URL),
            "zone {zone:?} : l'épisode terminé rend encore une position : {restantes:?}"
        );
        // L'autre épisode, commencé sur A, garde la sienne.
        assert_eq!(restantes.len(), 1, "zone {zone:?} : {restantes:?}");
        assert_eq!(restantes[0].audio_url, AUTRE);
        assert_eq!(restantes[0].position_ms, 300_000);
    }
    let lignes = db
        .query_many(
            "SELECT zone_id FROM podcast_episode_positions WHERE episode_key = ?",
            &[&cle_d_episode(URL) as &dyn ToSqlValue],
        )
        .unwrap();
    assert!(lignes.is_empty(), "lignes restantes : {lignes:?}");
}

#[test]
fn un_episode_termine_sur_b_n_a_plus_de_position_sur_a_8170() {
    use crate::db::{migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zones = ZoneRepo::with_backend(db.clone());
    let a = zones.create("Salon", Some("browser"), None).unwrap();
    let b = zones.create("Bureau", Some("browser"), None).unwrap();
    verifier(db, a, b);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_un_episode_termine_sur_b_n_a_plus_de_position_sur_a_8170() {
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
    for script in [
        include_str!("../../migrations/postgres/100_listening_resumes.sql"),
        include_str!("../../migrations/postgres/116_podcast_episode_positions.sql"),
    ] {
        let debut = script.find("CREATE TABLE IF NOT EXISTS").unwrap();
        let fin = debut + script[debut..].find(");").unwrap() + 2;
        let ddl =
            script[debut..fin].replace("CREATE TABLE IF NOT EXISTS", "CREATE TEMPORARY TABLE");
        // `AssertSqlSafe` : DDL des scripts de migration du dépôt, aucune entrée.
        sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
            .execute(&pool)
            .await
            .unwrap();
    }
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    verifier(db, 1, 2);
    pool.close().await;
}
