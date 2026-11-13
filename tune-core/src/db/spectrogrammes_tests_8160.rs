//! #8160 — la trace datée des spectrogrammes conservés, en SQLite puis sur une
//! vraie base PostgreSQL (`TUNE_TEST_PG_URL`, SAUT sans elle).
//!
//! * `spectrogramme_enregistre_relu_remplace_8160` : une image s'enregistre,
//!   se relit, se remplace ; le remplacement rend le nom de l'ancienne ;
//! * `spectrogramme_oublie_avec_son_image_8160` : une image évincée emporte
//!   sa ligne, une piste retirée aussi (`ON DELETE CASCADE`) ;
//! * `signature_change_avec_le_fichier_8160` : réécrire le fichier change la
//!   signature ;
//! * `pg_8160_spectrogrammes` : les mêmes propriétés, script PG 114 compris.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::spectrogrammes::{self as spectro, Spectrogramme};
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

fn executer(b: &dyn DbBackend, sql: &str) {
    b.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

fn image(track_id: i64, fichier: &str, octets: i64) -> Spectrogramme {
    Spectrogramme {
        track_id,
        fichier: fichier.to_string(),
        largeur: 1200,
        hauteur: 400,
        octets,
        signature: "1000:42".to_string(),
        taille_fichier: Some(1000),
        mtime_fichier: Some(1_762_000_000.25),
        duree_calcul_ms: Some(812),
        calcule_le: "2026-11-09T10:00:00Z".to_string(),
    }
}

fn semer_pistes(b: &dyn DbBackend) {
    executer(
        b,
        "INSERT INTO tracks (id, title, file_path, source) VALUES \
         (1, 'une', '/m/1.flac', 'local'), (2, 'deux', '/m/2.flac', 'local')",
    );
}

fn verifier_enregistre_relu_remplace(b: &dyn DbBackend) {
    assert_eq!(spectro::lire(b, 1).unwrap(), None);
    let premiere = image(1, "1-aaaa-1200x400.png", 90_000);
    assert_eq!(spectro::enregistrer(b, &premiere).unwrap(), None);
    assert_eq!(spectro::lire(b, 1).unwrap(), Some(premiere.clone()));
    // Même image réenregistrée : rien à retirer du cache.
    assert_eq!(spectro::enregistrer(b, &premiere).unwrap(), None);
    // Le fichier a changé : nouvelle image, l'ancienne est rendue.
    let seconde = Spectrogramme {
        signature: "2000:43".into(),
        taille_fichier: Some(2000),
        calcule_le: "2026-11-09T11:00:00Z".into(),
        ..image(1, "1-bbbb-1200x400.png", 95_000)
    };
    assert_eq!(
        spectro::enregistrer(b, &seconde).unwrap().as_deref(),
        Some("1-aaaa-1200x400.png"),
        "remplacer l'image d'une piste doit rendre le nom de la précédente"
    );
    assert_eq!(spectro::lire(b, 1).unwrap(), Some(seconde));
}

fn verifier_oubli(b: &dyn DbBackend) {
    spectro::enregistrer(b, &image(1, "1-x.png", 10)).unwrap();
    spectro::enregistrer(b, &image(2, "2-y.png", 10)).unwrap();
    assert_eq!(
        spectro::oublier_les_fichiers(b, &["1-x.png".into(), "inconnu.png".into()]).unwrap(),
        1
    );
    assert_eq!(spectro::lire(b, 1).unwrap(), None);
    assert!(spectro::lire(b, 2).unwrap().is_some());
    assert!(spectro::oublier(b, 2).unwrap());
    assert!(!spectro::oublier(b, 2).unwrap());
}

#[test]
fn spectrogramme_enregistre_relu_remplace_8160() {
    let b = base();
    semer_pistes(b.as_ref());
    verifier_enregistre_relu_remplace(b.as_ref());
}

#[test]
fn spectrogramme_oublie_avec_son_image_8160() {
    let b = base();
    semer_pistes(b.as_ref());
    verifier_oubli(b.as_ref());
    // Une piste retirée emporte sa ligne.
    spectro::enregistrer(b.as_ref(), &image(2, "2-z.png", 10)).unwrap();
    executer(b.as_ref(), "DELETE FROM tracks WHERE id = 2");
    assert_eq!(spectro::lire(b.as_ref(), 2).unwrap(), None);
}

#[test]
fn signature_change_avec_le_fichier_8160() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("a.flac");
    std::fs::write(&chemin, b"abc").unwrap();
    let avant = spectro::signature(&std::fs::metadata(&chemin).unwrap());
    assert!(avant.starts_with("3:"), "{avant}");
    std::fs::write(&chemin, b"abcdef").unwrap();
    let apres = spectro::signature(&std::fs::metadata(&chemin).unwrap());
    assert_ne!(avant, apres);
    assert!(apres.starts_with("6:"), "{apres}");
}

// ── PostgreSQL ──────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
async fn pg_8160_script_114(pool: &sqlx::PgPool) {
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/114_spectrogrammes.sql"
    ))
    .execute(pool)
    .await
    .unwrap_or_else(|e| panic!("script PG 114 : {e}"));
}

/// Joué contre une vraie base (`TUNE_TEST_PG_URL`, schéma des scripts
/// numérotés). Une seule épreuve, en série : elle vide les tables qu'elle lit.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8160_spectrogrammes() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    let b: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    // Base d'avant la 114 : la table retirée, sa marque aussi.
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS spectrogrammes;
         DELETE FROM schema_version WHERE version = 114;
         TRUNCATE TABLE tracks RESTART IDENTITY CASCADE;",
    )
    .execute(&pool)
    .await
    .unwrap();
    pg_8160_script_114(&pool).await;
    // Rejouable sans effet.
    pg_8160_script_114(&pool).await;
    semer_pistes(b.as_ref());
    verifier_enregistre_relu_remplace(b.as_ref());
    executer(b.as_ref(), "DELETE FROM spectrogrammes");
    verifier_oubli(b.as_ref());
    spectro::enregistrer(b.as_ref(), &image(2, "2-z.png", 10)).unwrap();
    executer(b.as_ref(), "DELETE FROM tracks WHERE id = 2");
    assert_eq!(
        spectro::lire(b.as_ref(), 2).unwrap(),
        None,
        "ON DELETE CASCADE : une piste retirée emporte sa ligne"
    );
}
