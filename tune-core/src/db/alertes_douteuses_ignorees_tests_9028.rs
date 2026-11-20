//! #9028 — le geste groupé « Normal pour les N pistes de cet album » écrit
//! une ligne PAR PISTE, dans une transaction (tout ou rien), et son
//! annulation retire exactement les lignes ajoutées. SQLite, et PostgreSQL
//! (`pg_9028_…`, saute sans `TUNE_TEST_PG_URL`, étape dédiée dans
//! `test-postgres.yml`).

use super::*;
use crate::db::backend::DbBackend;

fn alerte(cle: &str) -> AlerteIgnoree {
    AlerteIgnoree {
        portee: Portee::Piste,
        cle: cle.to_string(),
        titre: String::new(),
        raison: "very_short".to_string(),
        cible_id: None,
        libelle: Some("x".to_string()),
        ignoree_le: 1,
    }
}

fn cle_de(a: &AlerteIgnoree) -> (Portee, String, String, String) {
    (a.portee, a.cle.clone(), a.titre.clone(), a.raison.clone())
}

/// Ajoute, rejoue (rien d'ajouté), retire exactement ce qu'on nomme.
fn lot_ajoute_puis_retire_exactement(db: &Arc<dyn DbBackend>, prefixe: &str) {
    let seule = alerte(&format!("{prefixe}/02.flac"));
    assert_eq!(
        ignorer_lot(db, std::slice::from_ref(&seule)).unwrap(),
        vec![true]
    );
    let lot: Vec<AlerteIgnoree> = ["02", "03", "04"]
        .iter()
        .map(|n| alerte(&format!("{prefixe}/{n}.flac")))
        .collect();
    let ajoutees = ignorer_lot(db, &lot).unwrap();
    assert_eq!(ajoutees, vec![false, true, true], "02 l'était déjà");
    assert_eq!(ignorer_lot(db, &lot).unwrap(), vec![false, false, false]);
    // L'annulation ne vise que les lignes AJOUTÉES par le geste.
    let a_retirer: Vec<_> = lot
        .iter()
        .zip(&ajoutees)
        .filter(|(_, a)| **a)
        .map(|(l, _)| cle_de(l))
        .collect();
    assert_eq!(retablir_lot(db, &a_retirer).unwrap(), 2);
    assert_eq!(retablir_lot(db, &a_retirer).unwrap(), 0, "rejouée");
    let restantes: Vec<String> = lister(db)
        .unwrap()
        .into_iter()
        .filter(|a| a.cle.starts_with(prefixe))
        .map(|a| a.cle)
        .collect();
    assert_eq!(
        restantes,
        vec![format!("{prefixe}/02.flac")],
        "le choix d'avant reste"
    );
    retablir_lot(db, &[cle_de(&seule)]).unwrap();
}

fn sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn lot_sqlite() {
    lot_ajoute_puis_retire_exactement(&sqlite(), "/m/9028");
}

/// Tout ou rien : une ligne refusée au milieu du lot n'en laisse aucune.
#[test]
fn lot_sqlite_tout_ou_rien() {
    let db = sqlite();
    db.execute_batch(
        "CREATE TRIGGER refus_9028 BEFORE INSERT ON alertes_douteuses_ignorees \
         WHEN NEW.cle = '/m/boum.flac' BEGIN SELECT RAISE(ABORT, 'boum'); END;",
    )
    .unwrap();
    let lot = [
        alerte("/m/a.flac"),
        alerte("/m/boum.flac"),
        alerte("/m/c.flac"),
    ];
    assert!(ignorer_lot(&db, &lot).is_err());
    assert!(lister(&db).unwrap().is_empty(), "rien n'est resté");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_9028_lot_par_piste() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let nettoyer = || {
        db.execute(
            "DELETE FROM alertes_douteuses_ignorees WHERE cle LIKE '/pg9028/%'",
            &[],
        )
        .unwrap();
        db.execute("DELETE FROM tracks WHERE id BETWEEN 902800 AND 902809", &[])
            .unwrap();
        db.execute("DELETE FROM albums WHERE id = 902800", &[])
            .unwrap();
    };
    nettoyer();

    lot_ajoute_puis_retire_exactement(&db, "/pg9028/lot");

    // Tout ou rien : PostgreSQL refuse l'octet nul dans un texte.
    let lot = [
        alerte("/pg9028/a.flac"),
        alerte("/pg9028/b\0.flac"),
        alerte("/pg9028/c.flac"),
    ];
    assert!(ignorer_lot(&db, &lot).is_err());
    assert!(
        lister(&db)
            .unwrap()
            .iter()
            .all(|a| !a.cle.starts_with("/pg9028/")),
        "rien n'est resté"
    );

    // Le filtre SQL reconnaît une ligne de piste CUE (`chemin#début`) sur
    // PostgreSQL : la piste couverte sort, sa voisine reste.
    db.execute(
        "INSERT INTO albums (id, title) VALUES (902800, 'Everyday Life 9028')",
        &[],
    )
    .unwrap();
    for (id, debut) in [(902801i64, 240_000i64), (902802, 243_000)] {
        db.execute(
            "INSERT INTO tracks (id, title, album_id, duration_ms, source, cue_media_path, cue_start_ms) \
             VALUES (?, 'cachée', 902800, 3000, 'local', '/pg9028/EL/image.flac', ?)",
            &[&id as &dyn crate::db::backend::ToSqlValue, &debut],
        )
        .unwrap();
    }
    let compte = |db: &Arc<dyn DbBackend>| {
        db.query_one(
            &format!(
                "SELECT COUNT(*) FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE t.id BETWEEN 902800 AND 902809 AND {}",
                clause_non_ignoree("very_short")
            ),
            &[],
        )
        .unwrap()
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
        .unwrap()
    };
    assert_eq!(compte(&db), 2);
    let ajoutees = ignorer_lot(&db, &[alerte("/pg9028/EL/image.flac#240000")]).unwrap();
    assert_eq!(ajoutees, vec![true]);
    assert_eq!(compte(&db), 1, "seule la piste nommée est couverte");
    nettoyer();
}
