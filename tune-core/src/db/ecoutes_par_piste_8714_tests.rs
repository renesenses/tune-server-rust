//! #8714 (Procol Harum, 1967, fil 2806) — le nombre d'écoutes d'un titre
//! était partagé par toutes ses copies du même interprète (autre album,
//! compilation) : `listen_history.track_id` n'était jamais écrit, et une
//! écoute se rapprochait d'une piste par titre + interprète seulement.
//!
//! Règle tenue ici, sur SQLite ET PostgreSQL (`pg_8714_…`, saute sans
//! `TUNE_TEST_PG_URL`), par la colonne de la Bibliothèque
//! (`plays_for_tracks`) et par les listes intelligentes
//! (`ecoutes_de_la_piste`) :
//!
//! 1. une écoute qui désigne une piste ne compte que pour elle ;
//! 2. sans piste, une écoute dont l'album est connu ne compte que pour le
//!    titre de cet album ;
//! 3. l'ancien historique, sans piste ni album, garde le titre + interprète.
//!
//! Contre-épreuve : la règle d'avant (titre + interprète seuls) donne 1 à la
//! copie de la compilation dès la première écoute.

use super::*;
use crate::db::migrations;

const TITRE: &str = "Homburg 8714";
const INTERPRETE: &str = "Procol Harum 8714";

/// Deux albums de `base` et `base + 1`, chacun avec SA piste « Homburg » :
/// la piste `base` (l'album de 1967) et `base + 1` (une compilation).
fn poser(db: &Arc<dyn DbBackend>, base: i64) {
    db.execute(
        &format!("INSERT INTO artists (id, name) VALUES ({base}, '{INTERPRETE}')"),
        &[],
    )
    .unwrap();
    for (decalage, album) in [(0, "Procol Harum 1967"), (1, "Best Of")] {
        let id = base + decalage;
        db.execute(
            &format!(
                "INSERT INTO albums (id, title, artist_id, track_count) \
                 VALUES ({id}, '{album}', {base}, 1)"
            ),
            &[],
        )
        .unwrap();
        db.execute(
            &format!(
                "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path, source) \
                 VALUES ({id}, '{TITRE}', {base}, {id}, 1, '/musique/8714-{decalage}.flac', 'local')"
            ),
            &[],
        )
        .unwrap();
    }
    let reprise = base + 2;
    db.execute(
        &format!(
            "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path, source) \
         VALUES ({reprise}, '{TITRE}', {base}, {base}, 2, '/musique/8714-reprise.flac', 'local')"
        ),
        &[],
    )
    .unwrap();
}

fn ecouter(db: &Arc<dyn DbBackend>, track_id: Option<i64>, album_id: Option<i64>) {
    HistoryRepo::with_backend(db.clone())
        .record(&ListenRecord {
            track_id,
            album_id,
            title: TITRE.into(),
            artist_name: Some(INTERPRETE.into()),
            source: "local".into(),
            duration_ms: 240_000,
            ..Default::default()
        })
        .unwrap();
}

/// Le compte de la piste, par la colonne de la Bibliothèque ET par les listes
/// intelligentes, qui doivent s'accorder.
fn compte(db: &Arc<dyn DbBackend>, piste: i64) -> i64 {
    let colonne = HistoryRepo::with_backend(db.clone())
        .plays_for_tracks(&[piste])
        .unwrap()[&piste]
        .0;
    let intelligentes = db
        .query_one(
            &format!(
                "SELECT {} FROM tracks t WHERE t.id = {piste}",
                sql::ecoutes_de_la_piste("t")
            ),
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    assert_eq!(
        intelligentes, colonne,
        "listes intelligentes et Bibliothèque s'accordent"
    );
    colonne
}

fn verifier(db: &Arc<dyn DbBackend>, base: i64) {
    let (album_1967, compilation) = (base, base + 1);

    // 1. Écoute de la piste de l'album de 1967 (`track_id` écrit) : la
    //    compilation n'en reçoit rien.
    ecouter(db, Some(album_1967), Some(album_1967));
    assert_eq!(compte(db, album_1967), 1);
    assert_eq!(
        compte(db, compilation),
        0,
        "#8714 : l'écoute d'un titre a compté pour sa copie d un autre album"
    );

    // Précision du testeur du 14/11 : même titre ET même album, deux pistes.
    let reprise = base + 2;
    assert_eq!(
        compte(db, reprise),
        0,
        "#8714 : la reprise homonyme du même album ne doit pas compter l'autre piste"
    );
    ecouter(db, Some(reprise), Some(album_1967));
    assert_eq!(compte(db, reprise), 1);
    assert_eq!(
        compte(db, album_1967),
        1,
        "#8714 : l'écoute de la reprise ne doit pas incrémenter la première piste"
    );
    assert_eq!(compte(db, compilation), 0);

    // 2. Écoute sans piste mais avec son album (lignes d'avant #8714) : même
    //    séparation.
    ecouter(db, None, Some(compilation));
    assert_eq!(compte(db, album_1967), 1);
    assert_eq!(compte(db, compilation), 1);

    // 3. Ancien historique sans piste ni album : titre + interprète, les deux.
    ecouter(db, None, None);
    assert_eq!(compte(db, album_1967), 2);
    assert_eq!(compte(db, compilation), 2);

    // 4. La piste explicite prime sur les métadonnées de l'album conservées.
    ecouter(db, Some(compilation), Some(album_1967));
    assert_eq!(compte(db, album_1967), 2);
    assert_eq!(compte(db, compilation), 3);

    // 5. Une écoute de service homonyme ne devient pas une écoute locale.
    HistoryRepo::with_backend(db.clone())
        .record(&ListenRecord {
            title: TITRE.into(),
            artist_name: Some(INTERPRETE.into()),
            source: "qobuz".into(),
            duration_ms: 240_000,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(compte(db, album_1967), 2);
    assert_eq!(compte(db, compilation), 3);

    // 6. Un titre homonyme d'un autre interprète ne devient pas la même
    // piste parce qu'une ancienne ligne porte aussi cet album (compilation).
    for artiste in [Some("Autre interprète 8714"), None] {
        HistoryRepo::with_backend(db.clone())
            .record(&ListenRecord {
                album_id: Some(album_1967),
                title: TITRE.into(),
                artist_name: artiste.map(Into::into),
                source: "local".into(),
                duration_ms: 240_000,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            compte(db, album_1967),
            if artiste.is_some() { 2 } else { 3 },
            "#8714 : l'album ne doit pas faire compter un interprète explicitement différent"
        );
        assert_eq!(compte(db, compilation), 3);
    }
}

#[test]
fn sqlite_une_ecoute_ne_compte_que_pour_sa_piste_8714() {
    let base = SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    poser(&db, 1);
    verifier(&db, 1);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8714_une_ecoute_ne_compte_que_pour_sa_piste() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let postgres = crate::db::postgres::PostgresDb::connect(&url)
        .await
        .expect("connexion PG");
    crate::db::migrations::run_pg_migrations(postgres.pool())
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    // Comme au premier démarrage : les migrations créent les tables, puis
    // ensure_schema pose les colonnes complémentaires, dont source_id.
    postgres.ensure_schema().await;
    let pool = postgres.pool().clone();
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let nettoyer = |db: &Arc<dyn DbBackend>| {
        for sql in [
            format!("DELETE FROM listen_history WHERE title = '{TITRE}'"),
            "DELETE FROM tracks WHERE id BETWEEN 8714 AND 8716".to_string(),
            "DELETE FROM albums WHERE id BETWEEN 8714 AND 8715".to_string(),
            "DELETE FROM artists WHERE id = 8714".to_string(),
        ] {
            db.execute(&sql, &[]).unwrap();
        }
    };
    nettoyer(&db);
    poser(&db, 8714);
    verifier(&db, 8714);
    nettoyer(&db);
}
