//! #8325 — la découpe et le cumul des facettes classiques, sans base.

use super::*;

#[test]
fn une_valeur_jointe_se_decoupe_en_entrees() {
    let e: Vec<&str> = entrees(" Chœur ; Orchestre;; Soliste ").collect();
    assert_eq!(e, vec!["Chœur", "Orchestre", "Soliste"]);
}

#[test]
fn le_cumul_compte_une_piste_une_fois_par_entree() {
    let rows = vec![
        (1, "Berliner Philharmoniker".to_string()),
        (1, "berliner philharmoniker".to_string()),
        (2, "Chœur; Berliner Philharmoniker".to_string()),
        (3, "Chœur".to_string()),
    ];
    assert_eq!(
        cumuler(&rows, None),
        vec![
            ("Berliner Philharmoniker".to_string(), 2),
            ("Chœur".to_string(), 2),
        ]
    );
    assert_eq!(cumuler(&rows, Some(1)).len(), 1);
}

#[test]
fn les_sources_dependent_du_champ() {
    let chef = paires(Engine::Sqlite, Champ::Chef);
    assert!(chef.contains("track_credits") && chef.contains("album_metadata"));
    let oeuvre = paires(Engine::Sqlite, Champ::Oeuvre);
    assert!(!oeuvre.contains("track_credits"));
    let mouvement = paires(Engine::Sqlite, Champ::Mouvement);
    assert!(!mouvement.contains("album_metadata"));
    assert_eq!(
        Champ::facette("movement"),
        None,
        "le mouvement n'est pas une facette"
    );
    assert_eq!(Champ::facette("work"), Some(Champ::Oeuvre));
}

/// Le même parcours sur le VRAI moteur PostgreSQL, migrations comprises :
/// effectifs du rail, prédicat de la liste (marqueurs `$n`, identifiants
/// projetés en texte) et colonnes. Saute sans `TUNE_TEST_PG_URL`.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_chefs_ensembles_et_oeuvres_8325() {
    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::db::facet_filter::Placeholders;
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
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
    tokio::task::spawn_blocking(move || {
        for t in [
            "track_credits",
            "track_metadata",
            "album_metadata",
            "tracks",
            "albums",
        ] {
            let _ = db.execute(&format!("TRUNCATE TABLE {t} RESTART IDENTITY CASCADE"), &[]);
        }
        db.execute("INSERT INTO albums (title) VALUES ('Requiem')", &[])
            .unwrap();
        let album: i64 = db.query_many("SELECT id FROM albums", &[]).unwrap()[0][0]
            .as_i64()
            .unwrap();
        let mut pistes = Vec::new();
        for titre in ["A", "B"] {
            db.execute(
                "INSERT INTO tracks (title, file_path, album_id) VALUES ($1, $2, $3)",
                &[
                    &titre as &dyn ToSqlValue,
                    &format!("/m/{titre}.flac") as &dyn ToSqlValue,
                    &album as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            let id = db
                .query_many(
                    "SELECT id FROM tracks WHERE title = $1",
                    &[&titre as &dyn ToSqlValue],
                )
                .unwrap()[0][0]
                .as_i64()
                .unwrap();
            pistes.push(id);
        }
        let (a, b) = (pistes[0], pistes[1]);
        db.execute(
            "INSERT INTO track_credits (track_id, artist_name, role, position) \
             VALUES ($1, 'Herbert von Karajan', 'conductor', 0), \
                    ($1, 'Wiener Philharmoniker', 'performer', 1)",
            &[&a as &dyn ToSqlValue],
        )
        .unwrap();
        db.execute(
            "INSERT INTO track_metadata (track_id, key, value) \
             VALUES ($1, 'ensemble', 'Rundfunkchor Berlin; Wiener Philharmoniker')",
            &[&b as &dyn ToSqlValue],
        )
        .unwrap();
        db.execute(
            "INSERT INTO album_metadata (album_id, key, value) VALUES ($1, 'work', 'Requiem')",
            &[&album as &dyn ToSqlValue],
        )
        .unwrap();

        let ens = effectifs(&db, Champ::Ensemble, &[], &[], None).unwrap();
        assert_eq!(
            ens,
            vec![
                ("Wiener Philharmoniker".to_string(), 2),
                ("Rundfunkchor Berlin".to_string(), 1)
            ],
            "PG : effectifs des ensembles"
        );
        assert_eq!(
            effectifs(&db, Champ::Oeuvre, &[], &[], None).unwrap(),
            vec![("Requiem".to_string(), 2)]
        );

        let mut ph = Placeholders::new(Engine::Postgres);
        let (cond, liees) =
            bloc(Engine::Postgres, Champ::Chef)(&mut ph, &["herbert von karajan".into()]).unwrap();
        let bound: Vec<&dyn ToSqlValue> = liees.iter().map(|v| v as &dyn ToSqlValue).collect();
        let rangs = db
            .query_many(
                &format!("SELECT t.title FROM tracks t WHERE {cond}"),
                &bound,
            )
            .unwrap();
        let titres: Vec<String> = rangs
            .into_iter()
            .filter_map(|r| r.into_iter().next()?.as_string())
            .collect();
        assert_eq!(titres, vec!["A".to_string()], "PG : filtre par chef");

        let col = valeurs_des_pistes(&db, Champ::Ensemble, &[a, b]).unwrap();
        assert_eq!(
            col.get(&a).map(String::as_str),
            Some("Wiener Philharmoniker")
        );
        assert_eq!(
            col.get(&b).map(String::as_str),
            Some("Rundfunkchor Berlin; Wiener Philharmoniker")
        );
    })
    .await
    .unwrap();
}
