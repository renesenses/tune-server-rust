//! #3924: the aggregate and its provenance use the same contributing rows.
//! #4186: a third producer, the neighbouring `foo_dr.txt` report, has its own
//! column — it must never be counted as `unknown`.
use super::album_repo::AlbumRepo;
use super::backend::DbBackend;
use std::sync::Arc;

fn verify(db: Arc<dyn DbBackend>) {
    db.execute(
        "CREATE TEMP TABLE tracks (id BIGINT PRIMARY KEY, album_id BIGINT)",
        &[],
    )
    .unwrap();
    db.execute("CREATE TEMP TABLE track_metadata (track_id BIGINT, key TEXT, value TEXT, PRIMARY KEY(track_id,key))", &[]).unwrap();
    for id in 1_i64..=4 {
        db.execute("INSERT INTO tracks (id,album_id) VALUES (?,1)", &[&id])
            .unwrap();
    }
    let repo = AlbumRepo::with_backend(db.clone());
    let set = |id: i64, key: &str, value: &str| {
        db.execute("INSERT INTO track_metadata (track_id,key,value) VALUES (?,?,?) ON CONFLICT(track_id,key) DO UPDATE SET value=excluded.value", &[&id,&key,&value]).unwrap();
    };
    assert_eq!(repo.dynamic_range_detail(1).unwrap(), None);
    set(1, "dr_track", "10");
    set(2, "dr_track", "14");
    // (source piste 1, source piste 2, verdict, (tag, analysis, sidecar, unknown))
    for (a, b, kind, counts) in [
        ("tag", "tag", "tag", (2, 0, 0, 0)),
        ("analysis", "analysis", "analysis", (0, 2, 0, 0)),
        ("tag", "analysis", "mixed", (1, 1, 0, 0)),
        ("", "analysis", "unknown", (0, 1, 0, 1)),
        ("future-producer", "tag", "unknown", (1, 0, 0, 1)),
        // #4186 — le rapport voisin est un producteur CONNU, pas « unknown ».
        ("sidecar", "sidecar", "sidecar", (0, 0, 2, 0)),
        ("sidecar", "tag", "mixed", (1, 0, 1, 0)),
        ("sidecar", "analysis", "mixed", (0, 1, 1, 0)),
        ("sidecar", "", "unknown", (0, 0, 1, 1)),
    ] {
        set(1, "dr_source", a);
        set(2, "dr_source", b);
        let dr = repo.dynamic_range_detail(1).unwrap().unwrap();
        assert_eq!(dr.valeur, 12);
        assert_eq!(dr.source(), "track_average");
        assert_eq!(dr.provenance.source, kind, "sources ({a}, {b})");
        assert_eq!(
            (
                dr.provenance.tag_tracks,
                dr.provenance.analysis_tracks,
                dr.provenance.sidecar_tracks,
                dr.provenance.unknown_tracks
            ),
            counts,
            "sources ({a}, {b})"
        );
    }
    db.execute("DELETE FROM track_metadata WHERE key='dr_source'", &[])
        .unwrap();
    let dr = repo.dynamic_range_detail(1).unwrap().unwrap();
    assert_eq!(dr.provenance.unknown_tracks, 2);
    assert_eq!(dr.provenance.source, "unknown");
    set(1, "dr_source", "tag");
    set(2, "dr_source", "tag");
    set(3, "dr_track", "not-a-number");
    set(3, "dr_source", "analysis");
    set(4, "dr_source", "analysis");
    let dr = repo.dynamic_range_detail(1).unwrap().unwrap();
    assert_eq!(dr.valeur, 12);
    assert_eq!(dr.provenance.source, "tag");
    assert_eq!(dr.provenance.tag_tracks, 2);
    assert_eq!(dr.provenance.analysis_tracks, 0);
    set(3, "dr_album", "0");
    let dr = repo.dynamic_range_detail(1).unwrap().unwrap();
    assert_eq!(dr.valeur, 0);
    assert_eq!(dr.source(), "album_tag");
    assert_eq!(dr.provenance.source, "tag");
    assert_eq!(
        (
            dr.provenance.tag_tracks,
            dr.provenance.analysis_tracks,
            dr.provenance.sidecar_tracks,
            dr.provenance.unknown_tracks
        ),
        (0, 0, 0, 0)
    );
    db.execute("DELETE FROM track_metadata", &[]).unwrap();
    set(1, "dr_track", "bad");
    set(1, "dr_source", "tag");
    assert_eq!(repo.dynamic_range_detail(1).unwrap(), None);
}

/// #4521 — la liste `GET /library/albums` lit le DR de sa page par
/// `dynamic_range_by_ids`. Elle doit dire, album par album, EXACTEMENT ce que
/// dit la fiche (`dynamic_range_detail`) : même valeur, même absence.
fn verify_4521(db: Arc<dyn DbBackend>) {
    db.execute(
        "CREATE TEMP TABLE tracks (id BIGINT PRIMARY KEY, album_id BIGINT)",
        &[],
    )
    .unwrap();
    db.execute("CREATE TEMP TABLE track_metadata (track_id BIGINT, key TEXT, value TEXT, PRIMARY KEY(track_id,key))", &[]).unwrap();
    // (piste, album, clé, valeur) — `None` : une piste sans aucun DR.
    let lignes: [(i64, i64, Option<(&str, &str)>); 9] = [
        // 1 : tag d'album 9, qui PRIME sur le DR de piste 7.
        (1, 1, Some(("dr_album", "9"))),
        (2, 1, Some(("dr_track", "7"))),
        // 2 : aucune étiquette d'album, pistes 12 (tag) et 14 (analyse) → 13.
        (3, 2, Some(("dr_track", "12"))),
        (4, 2, Some(("dr_track", "14"))),
        // 3 : rien du tout — le cas le plus courant.
        (5, 3, None),
        // 4 : DR0, une MESURE (master écrasé), pas une absence.
        (6, 4, Some(("dr_album", "0"))),
        // 5 : ce que `normalise_dr` recopie tel quel — ni fiche ni liste.
        (7, 5, Some(("dr_album", "DR12.5"))),
        // 6 : une piste illisible ne compte pas, l'autre donne 11.
        (8, 6, Some(("dr_track", "bad"))),
        (9, 6, Some(("dr_track", "11"))),
    ];
    for (piste, album, tag) in lignes {
        db.execute(
            "INSERT INTO tracks (id,album_id) VALUES (?,?)",
            &[&piste, &album],
        )
        .unwrap();
        if let Some((k, v)) = tag {
            db.execute(
                "INSERT INTO track_metadata (track_id,key,value) VALUES (?,?,?)",
                &[&piste, &k, &v],
            )
            .unwrap();
        }
    }
    db.execute(
        "INSERT INTO track_metadata (track_id,key,value) VALUES (4,'dr_source','analysis')",
        &[],
    )
    .unwrap();
    let repo = AlbumRepo::with_backend(db.clone());
    let ids: Vec<i64> = (1..=6).collect();
    let liste = repo.dynamic_range_by_ids(&ids).unwrap();
    let attendu: std::collections::HashMap<i64, i64> =
        [(1, 9), (2, 13), (4, 0), (6, 11)].into_iter().collect();
    assert_eq!(liste, attendu, "le DR de la liste");
    for id in ids {
        assert_eq!(
            liste.get(&id).copied(),
            repo.dynamic_range_detail(id).unwrap().map(|d| d.valeur),
            "album {id} : la liste et la fiche divergent"
        );
    }
    // Bornée à la page : un identifiant non demandé ne sort pas.
    let page = repo.dynamic_range_by_ids(&[2, 3]).unwrap();
    assert_eq!(page.keys().copied().collect::<Vec<_>>(), vec![2]);
    assert!(repo.dynamic_range_by_ids(&[]).unwrap().is_empty());
}

#[test]
fn i4521_dr_liste_sqlite() {
    verify_4521(Arc::new(super::sqlite::SqliteDb::open_in_memory().unwrap()));
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn i4521_dr_liste_postgres() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    verify_4521(Arc::new(super::backend::PostgresBackend::new(pool.clone())));
    pool.close().await;
}

#[test]
fn i3924_album_provenance_sqlite() {
    verify(Arc::new(super::sqlite::SqliteDb::open_in_memory().unwrap()));
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn i3924_album_provenance_postgres() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    // One connection keeps these temporary tables isolated from all other tests.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    verify(Arc::new(super::backend::PostgresBackend::new(pool.clone())));
    pool.close().await;
}
