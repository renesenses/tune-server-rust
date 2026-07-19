//! End-to-end smoke tests for the `Arc<dyn DbBackend>` path running
//! against a real PostgreSQL instance.
//!
//! Gated on the `postgres` feature AND the `TUNE_TEST_PG_URL` env var.
//! Without that env var the tests are skipped — they're not part of
//! the default `cargo test` run.
//!
//! Run via `scripts/pg-e2e.sh` (spins up a disposable docker pg, applies
//! the migrations, exports the env var, then runs cargo).
//!
//! The tests intentionally focus on exercising the trait boundary —
//! one per repo, hitting `create` + one read path. Comprehensive
//! coverage stays in the SQLite tests; PG E2E proves the bridge.

#![cfg(all(test, feature = "postgres"))]

use std::sync::Arc;

use crate::db::backend::{DbBackend, PostgresBackend};

/// Connect to the test PG instance pointed at by `TUNE_TEST_PG_URL`.
/// Returns `None` when the env var is unset — caller short-circuits
/// so the test is a no-op on default `cargo test`.
async fn pg_backend() -> Option<Arc<dyn DbBackend>> {
    let url = std::env::var("TUNE_TEST_PG_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.ok()?;
    Some(Arc::new(PostgresBackend::new(pool)))
}

/// Test-time guard that bails out cleanly when no PG is wired up.
/// Use as `let db = pg_or_skip!();` at the top of every #[tokio::test]
/// test function. Must be called inside a Tokio runtime because the
/// PostgresBackend methods use `block_in_place` + `block_on` and
/// expect to be reached from one.
macro_rules! pg_or_skip {
    () => {
        match pg_backend().await {
            Some(db) => db,
            None => {
                eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
                return;
            }
        }
    };
}

/// Truncate every table the tests touch so each test starts clean.
/// CASCADE handles the FK chain.
fn reset_schema(db: &Arc<dyn DbBackend>) {
    let tables = [
        "track_credits",
        "play_queue",
        "playlist_tracks",
        "playlists",
        "tracks",
        "albums",
        "artists",
        "zones",
        "listen_history",
    ];
    for table in tables {
        let sql = format!("TRUNCATE TABLE {table} RESTART IDENTITY CASCADE");
        // ignore errors for tables that don't exist (older migration state)
        let _ = db.execute(&sql, &[]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_artists_round_trip() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::Artist;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = ArtistRepo::with_backend(db);

    let id = repo.create(&Artist::new("Miles Davis".into())).unwrap();
    assert!(id > 0);

    let fetched = repo.get(id).unwrap().unwrap();
    assert_eq!(fetched.name, "Miles Davis");

    let by_name = repo.get_by_name("miles davis").unwrap();
    assert_eq!(by_name.and_then(|a| a.id), Some(id));
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_albums_round_trip() {
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Album, Artist};

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let aid = artist_repo.create(&Artist::new("Coltrane".into())).unwrap();

    let repo = AlbumRepo::with_backend(db);
    let mut album = Album::new("A Love Supreme".into());
    album.artist_id = Some(aid);
    album.year = Some(1965);
    let id = repo.create(&album).unwrap();

    let fetched = repo.get(id).unwrap().unwrap();
    assert_eq!(fetched.title, "A Love Supreme");
    assert_eq!(fetched.artist_name.as_deref(), Some("Coltrane"));

    // get_or_create — the read-then-write path that's specific to album.
    let again = repo
        .get_or_create("A Love Supreme", aid, Some(1965))
        .unwrap();
    assert_eq!(again.id, Some(id));
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_tracks_round_trip() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let aid = artist_repo
        .create(&Artist::new("Pink Floyd".into()))
        .unwrap();

    let repo = TrackRepo::with_backend(db);
    let mut track = Track::new("Time".into());
    track.artist_id = Some(aid);
    track.file_path = Some("/music/time.flac".into());
    track.duration_ms = 413_000;
    let id = repo.create(&track).unwrap();

    let fetched = repo.get(id).unwrap().unwrap();
    assert_eq!(fetched.title, "Time");
    assert_eq!(fetched.duration_ms, 413_000);

    // get_all_paths used to be sqlite_legacy — now goes through DbBackend.
    let paths = repo.get_all_paths().unwrap();
    assert!(paths.contains("/music/time.flac"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_zones_round_trip() {
    use crate::db::zone_repo::ZoneRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = ZoneRepo::with_backend(db);

    let id = repo
        .create("Living Room", Some("dlna"), Some("uuid:1"))
        .unwrap();
    let z = repo.get(id).unwrap().unwrap();
    assert_eq!(z.name, "Living Room");
    assert_eq!(z.volume, 50);

    repo.update_volume(id, 75).unwrap();
    assert_eq!(repo.get(id).unwrap().unwrap().volume, 75);

    // The WAL fallback `query_many_strong` doesn't change behavior on
    // PG (same pool either way) — confirm list() works.
    let all = repo.list().unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_playlists_round_trip() {
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::playlist_repo::PlaylistRepo;
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let aid = artist_repo.create(&Artist::new("Test".into())).unwrap();

    let track_repo = TrackRepo::with_backend(db.clone());
    let mut t = Track::new("Song".into());
    t.artist_id = Some(aid);
    t.file_path = Some("/song.flac".into());
    let tid = track_repo.create(&t).unwrap();

    let repo = PlaylistRepo::with_backend(db);
    let plid = repo.create("My PL", None, 1).unwrap();
    // add_tracks uses write_tx — exercises the tx bridge.
    let inserted = repo.add_tracks(plid, &[tid], None).unwrap();
    assert_eq!(inserted, vec![tid]);

    let ids = repo.get_track_ids(plid).unwrap();
    assert_eq!(ids, vec![tid]);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_history_round_trip() {
    use crate::db::history_repo::{HistoryRepo, ListenRecord};

    let db = pg_or_skip!();
    reset_schema(&db);
    let repo = HistoryRepo::with_backend(db);

    let rec = ListenRecord {
        id: None,
        track_id: None,
        title: "So What".into(),
        artist_name: Some("Miles".into()),
        album_title: Some("Kind of Blue".into()),
        source: "local".into(),
        source_id: None,
        album_id: None,
        duration_ms: 560_000,
        listened_at: None,
        zone_id: None,
        cover_url: None,
        profile_id: None,
    };
    repo.record(&rec).unwrap();
    repo.record(&rec).unwrap();

    let recent = repo.recent(10).unwrap();
    assert_eq!(recent.len(), 2);

    let dashboard = repo.dashboard().unwrap();
    assert_eq!(dashboard.total_listens, 2);

    // listening_history uses the date helpers — confirms PG branch
    // of since_days / date_trunc_day.
    let days = repo.listening_history(7).unwrap();
    assert!(
        !days.is_empty(),
        "expected at least one day in 7-day window"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_settings_round_trip() {
    use crate::db::settings_repo::SettingsRepo;

    let db = pg_or_skip!();
    // settings table not in 001 — but settings_repo handles its own
    // schema bootstrap via the migration runner? No, the schema is
    // expected to be present. Skip if not.
    let exists = db
        .query_one(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'settings'",
            &[],
        )
        .unwrap_or(None);
    if exists.is_none() {
        eprintln!("settings table missing on PG — skipping");
        return;
    }
    let _ = db.execute("TRUNCATE TABLE settings", &[]);
    let repo = SettingsRepo::with_backend(db);

    repo.set("music_dirs", r#"["/music"]"#).unwrap();
    assert_eq!(
        repo.get("music_dirs").unwrap().as_deref(),
        Some(r#"["/music"]"#)
    );
    repo.delete("music_dirs").unwrap();
    assert!(repo.get("music_dirs").unwrap().is_none());
}
