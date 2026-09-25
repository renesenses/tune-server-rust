//! #5034 — la SOURCE de la pochette d'album sur une VRAIE base PostgreSQL.
//!
//! SQLite avale ce que PostgreSQL refuse : un marqueur répété, un type de
//! colonne, un `WHERE cover_path IS NULL` mal placé. Ce banc joue chaque
//! requête que le scan et le surveillant émettent sur `albums.cover_source`,
//! contre le schéma que la migration 074 a posé (étape « Apply PG
//! migrations » du workflow), dans une table TEMPORAIRE copiée de la vraie
//! (`LIKE albums INCLUDING ALL`) — rien n'est écrit dans la base partagée.
use super::album_repo::AlbumRepo;
use super::backend::DbBackend;
use super::models::{Album, SourcePochette};
use std::sync::Arc;

fn verifier(db: Arc<dyn DbBackend>) {
    let repo = AlbumRepo::with_backend(db.clone());
    let a = repo.create(&Album::new("Album de Didier".into())).unwrap();
    let b = repo.create(&Album::new("Doublon".into())).unwrap();

    // Pochette tirée du disque : source, fichier, empreinte.
    repo.poser_pochette_du_disque(
        a,
        "c1",
        SourcePochette::Dossier,
        "/m/A/cover.jpg",
        Some("9:42"),
    )
    .unwrap();
    let e = repo.etat_pochette(a).unwrap().unwrap();
    assert_eq!(e.cover_path.as_deref(), Some("c1"));
    assert_eq!(e.source, Some(SourcePochette::Dossier));
    assert_eq!(e.fichier.as_deref(), Some("/m/A/cover.jpg"));
    assert_eq!(e.empreinte.as_deref(), Some("9:42"));
    assert!(
        repo.pochettes_tirees_du_disque()
            .unwrap()
            .iter()
            .any(|(id, f, _)| *id == a && f == "/m/A/cover.jpg"),
        "la liste de fin de scan doit rendre l'album local"
    );

    // COALESCE : une pochette en place ne cède pas, et sa source non plus.
    repo.update_cover_path(a, "c2", SourcePochette::Fournisseur)
        .unwrap();
    let e = repo.etat_pochette(a).unwrap().unwrap();
    assert_eq!(
        (e.cover_path.as_deref(), e.source),
        (Some("c1"), Some(SourcePochette::Dossier))
    );

    // Téléversement : écrase, et oublie le fichier.
    repo.force_update_cover_path(a, "up", SourcePochette::Televersee)
        .unwrap();
    let e = repo.etat_pochette(a).unwrap().unwrap();
    assert_eq!(
        (e.cover_path.as_deref(), e.source, e.fichier),
        (Some("up"), Some(SourcePochette::Televersee), None)
    );

    // Retrait : tout part ensemble.
    repo.retirer_pochette(a).unwrap();
    assert_eq!(
        repo.etat_pochette(a).unwrap().unwrap(),
        super::album_repo::EtatPochette::default()
    );

    // Absorption : la source voyage avec la pochette (cinq marqueurs, `$1`…`$5`).
    repo.poser_pochette_du_disque(b, "cb", SourcePochette::Integree, "/m/B/01.flac", None)
        .unwrap();
    repo.absorber(a, b).unwrap();
    let e = repo.etat_pochette(a).unwrap().unwrap();
    assert_eq!(
        (e.cover_path.as_deref(), e.source, e.fichier.as_deref()),
        (
            Some("cb"),
            Some(SourcePochette::Integree),
            Some("/m/B/01.flac")
        )
    );
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_5034_source_de_pochette() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    // Une seule connexion : la table temporaire n'existe que pour elle.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(super::backend::PostgresBackend::new(pool.clone()));
    // Le schéma RÉEL, migrations appliquées : sans la 074, la copie n'a pas
    // les colonnes et la première requête échoue.
    db.execute(
        "CREATE TEMP TABLE albums (LIKE public.albums INCLUDING ALL)",
        &[],
    )
    .unwrap();
    for colonne in ["cover_source", "cover_source_path", "cover_source_stamp"] {
        let n = db
            .query_one(
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = 'albums' AND column_name = ?",
                &[&colonne],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        assert_eq!(n, 1, "la migration PG 074 n'a pas posé `albums.{colonne}`");
    }
    // Les tables que l'absorption repointe : vides, temporaires.
    db.execute(
        "CREATE TEMP TABLE tracks (LIKE public.tracks INCLUDING ALL)",
        &[],
    )
    .unwrap();
    // La 074 rejouée : idempotente.
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/074_albums_source_de_pochette.sql"
    ))
    .execute(&pool)
    .await
    .expect("la 074 doit se rejouer sans erreur");
    verifier(db);
    pool.close().await;
}

/// Le même banc sur SQLite : la contre-épreuve du moteur.
#[test]
fn sqlite_5034_source_de_pochette() {
    let db = super::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    verifier(Arc::new(db));
}
