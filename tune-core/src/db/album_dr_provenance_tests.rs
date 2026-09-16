//! #3924: the aggregate and its provenance use the same contributing rows.
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
    for (a, b, kind, counts) in [
        ("tag", "tag", "tag", (2, 0, 0)),
        ("analysis", "analysis", "analysis", (0, 2, 0)),
        ("tag", "analysis", "mixed", (1, 1, 0)),
        ("", "analysis", "unknown", (0, 1, 1)),
        ("future-producer", "tag", "unknown", (1, 0, 1)),
    ] {
        set(1, "dr_source", a);
        set(2, "dr_source", b);
        let dr = repo.dynamic_range_detail(1).unwrap().unwrap();
        assert_eq!(dr.valeur, 12);
        assert_eq!(dr.source(), "track_average");
        assert_eq!(dr.provenance.source, kind);
        assert_eq!(
            (
                dr.provenance.tag_tracks,
                dr.provenance.analysis_tracks,
                dr.provenance.unknown_tracks
            ),
            counts
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
            dr.provenance.unknown_tracks
        ),
        (0, 0, 0)
    );
    db.execute("DELETE FROM track_metadata", &[]).unwrap();
    set(1, "dr_track", "bad");
    set(1, "dr_source", "tag");
    assert_eq!(repo.dynamic_range_detail(1).unwrap(), None);
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
