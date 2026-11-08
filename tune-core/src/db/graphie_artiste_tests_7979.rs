//! #7979 — un nom d'artiste corrigé dans les balises est repris par la fiche,
//! sur SQLite et sur PostgreSQL.
//!
//! Fil forum 2664 : « Bireli Lagrene » corrigé en « Biréli Lagrène » dans les
//! fichiers et la feuille CUE restait « Bireli Lagrene » après toutes les
//! analyses. La fiche était retrouvée par la clé pliée (BIB-C2) et rendue
//! sans changer son nom.

use super::artist_repo::{ArtistRepo, graphie_plus_juste};
use super::backend::DbBackend;
use std::sync::Arc;

/// Ce que chaque moteur doit tenir, sur une table `artists` vide.
fn verifier(db: Arc<dyn DbBackend>) {
    let repo = ArtistRepo::with_backend(db.clone());
    let nom_en_base = |id: i64| {
        let ligne = db
            .query_one("SELECT name FROM artists WHERE id = ?", &[&id])
            .unwrap()
            .unwrap();
        ligne[0].as_string().unwrap()
    };

    // Cas 1 — les accents ajoutés dans les balises gagnent la fiche.
    let avant = repo.get_or_create("Bireli Lagrene", None, None).unwrap();
    let id = avant.id.unwrap();
    let apres = repo.get_or_create("Biréli Lagrène", None, None).unwrap();
    assert_eq!(apres.id, Some(id), "même artiste, même fiche");
    assert_eq!(
        apres.name, "Biréli Lagrène",
        "#7979 : la graphie accentuée des balises n'a pas été reprise"
    );
    assert_eq!(
        nom_en_base(id),
        "Biréli Lagrène",
        "la base garde l'ancien nom"
    );

    // Une piste encore sans accents ne fait pas revenir l'ancien nom : la
    // règle est à sens unique, la bibliothèque n'oscille pas.
    let encore = repo.get_or_create("Bireli Lagrene", None, None).unwrap();
    assert_eq!(encore.id, Some(id));
    assert_eq!(encore.name, "Biréli Lagrène");
    assert_eq!(nom_en_base(id), "Biréli Lagrène");

    // Cas 2 — l'espace ajouté donne un autre nom : une autre fiche, au nom
    // exact des balises.
    let colle = repo.get_or_create("JohnDoe Trio", None, None).unwrap();
    let espace = repo.get_or_create("John Doe Trio", None, None).unwrap();
    assert_ne!(espace.id, colle.id);
    assert_eq!(nom_en_base(espace.id.unwrap()), "John Doe Trio");

    // BIB-C2 reste : article et ponctuation retrouvent la fiche sans la
    // renommer.
    let stones = repo
        .get_or_create("The Rolling Stones", None, None)
        .unwrap();
    let sans_article = repo.get_or_create("Rolling Stones", None, None).unwrap();
    assert_eq!(sans_article.id, stones.id);
    assert_eq!(sans_article.name, "The Rolling Stones");
}

#[test]
fn graphie_7979_regle_a_sens_unique() {
    assert!(graphie_plus_juste("Bireli Lagrene", "Biréli Lagrène"));
    assert!(graphie_plus_juste("bireli lagrene", "Biréli Lagrène"));
    assert!(graphie_plus_juste("Biréli Lagrene", "Biréli Lagrène"));
    assert!(!graphie_plus_juste("Biréli Lagrène", "Bireli Lagrene"));
    assert!(!graphie_plus_juste("Biréli Lagrène", "Biréli Lagrène"));
    assert!(!graphie_plus_juste("Rolling Stones", "The Rolling Stones"));
    assert!(
        !graphie_plus_juste("Ayó", "Ayọ"),
        "autant d'accents : la fiche reste"
    );
}

#[test]
fn graphie_7979_artiste_renomme_sqlite() {
    let db = super::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    verifier(Arc::new(db));
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7979_artiste_renomme_postgres() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL absent");
        return;
    };
    // Une seule connexion : la table temporaire masque celle de la base de
    // test et disparaît avec elle.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS unaccent")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TEMPORARY TABLE artists (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, \
         sort_name TEXT, musicbrainz_id TEXT, discogs_id TEXT, bio TEXT, image_path TEXT, \
         image_source TEXT, sort_name_source TEXT, musicbrainz_id_source TEXT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(super::backend::PostgresBackend::new(pool.clone()));
    verifier(db);
    pool.close().await;
}
