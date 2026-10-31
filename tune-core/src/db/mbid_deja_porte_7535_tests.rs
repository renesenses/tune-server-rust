//! #7535 — « Phil Selway » et « Philip Selway » en deux fiches : la passe des
//! images (`library/artwork.rs`) et la phase MBID (`metadata/matcher.rs`)
//! cherchent le NOM sur MusicBrainz et posaient le MBID trouvé par
//! `ArtistRepo::poser_mbid`, sans regarder si une autre fiche le portait
//! déjà. Les deux fiches finissaient avec le même MBID, donc le même
//! portrait, sans jamais se réunir.
//!
//! Décision de Bertrand (31/10) : bloquer et proposer. Ici le blocage : un
//! MBID déjà porté par une AUTRE fiche n'est pas posé (journalisé). La
//! contre-épreuve : un MBID libre se pose, et une fiche qui porte déjà ce
//! MBID peut se le reposer.

use std::sync::Arc;

use super::artist_repo::{ArtistRepo, ProvenanceMbid};
use super::backend::DbBackend;

const PHILIP: &str = "a2d6f6d8-6a3c-4ec7-9b8d-3c6a0e9f7535";

fn mbid(repo: &ArtistRepo, id: i64) -> Option<String> {
    repo.get(id).unwrap().and_then(|a| a.musicbrainz_id)
}

/// Le scénario, joué sur le moteur que porte `repo`.
fn scenario(repo: &ArtistRepo) {
    let philip = repo
        .get_or_create("Philip Selway", Some(PHILIP), None)
        .unwrap()
        .id
        .unwrap();
    let phil = repo
        .get_or_create("Phil Selway", None, None)
        .unwrap()
        .id
        .unwrap();
    assert_ne!(philip, phil, "deux clés différentes : deux fiches");

    // La passe des images : le MBID trouvé par le nom, déjà porté (casse et
    // blancs différents : MusicBrainz et les balises ne s'accordent pas
    // toujours).
    let trouve = format!("  {}  ", PHILIP.to_uppercase());
    assert!(
        !repo.poser_mbid(phil, &trouve, ProvenanceMbid::Nom).unwrap(),
        "#7535 : un MBID déjà porté par « Philip Selway » posé sur « Phil Selway »"
    );
    assert_eq!(mbid(repo, phil), None);
    // La phase MBID du matcher passe par `update_mbid`.
    repo.update_mbid(phil, PHILIP).unwrap();
    assert_eq!(
        mbid(repo, phil),
        None,
        "#7535 : update_mbid a posé un MBID déjà porté"
    );
    // Ce que le journal nomme.
    assert_eq!(
        repo.autre_fiche_du_mbid(phil, PHILIP).unwrap(),
        Some((philip, "Philip Selway".to_string()))
    );
    // La fiche qui le porte reste intacte.
    assert_eq!(mbid(repo, philip).as_deref(), Some(PHILIP));

    // Contre-épreuves : la fiche qui porte déjà le MBID peut se le reposer,
    // et un MBID libre se pose.
    assert!(
        repo.poser_mbid(philip, PHILIP, ProvenanceMbid::Nom)
            .unwrap()
    );
    assert_eq!(repo.autre_fiche_du_mbid(philip, PHILIP).unwrap(), None);
    let libre = "0b0b0b0b-1111-2222-3333-444444447535";
    assert!(repo.poser_mbid(phil, libre, ProvenanceMbid::Nom).unwrap());
    assert_eq!(mbid(repo, phil).as_deref(), Some(libre));
    // Un MBID vide reste une absence, il ne « rencontre » personne.
    let gong = repo.get_or_create("Gong", None, None).unwrap().id.unwrap();
    repo.update_mbid(gong, "   ").unwrap();
    assert_eq!(mbid(repo, gong), None);
}

#[test]
fn un_mbid_trouve_par_le_nom_deja_porte_n_est_pas_pose_sqlite() {
    let db = super::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db);
    scenario(&ArtistRepo::with_backend(b));
}

/// La même garde sur PostgreSQL (`TUNE_TEST_PG_URL`) : `$4`/`$5` et le
/// `CAST(... AS TEXT)` de la garde. Dans un schéma à lui : le banc
/// PostgreSQL est partagé. SAUT sans URL.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_un_mbid_trouve_par_le_nom_deja_porte_n_est_pas_pose() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    const SCHEMA: &str = "mbid_deja_porte_7535";
    let maintenance = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA}"
    )))
    .execute(&maintenance)
    .await
    .unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {SCHEMA}")))
                    .execute(c)
                    .await
                    .map(|_| ())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/001_initial_schema.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(super::backend::PostgresBackend::new(pool.clone()));
    for sql in super::postgres::ENSURE_TABLES
        .iter()
        .chain(super::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = b.execute(sql, &[]);
    }
    scenario(&ArtistRepo::with_backend(b));
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
    )))
    .execute(&maintenance)
    .await
    .unwrap();
}
