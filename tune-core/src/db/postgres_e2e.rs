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

/// Preuve réelle sur le second dialecte pour #2458 : le MBID vide ne sert plus
/// d'identité et la réparation fail-closed exécute sa sélection + son UPDATE
/// dans une transaction PostgreSQL, pas seulement dans le fixture SQLite.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2458_empty_mbid_album_artist_repair() {
    use crate::db::album_repo::AlbumRepo;
    use crate::db::artist_repo::ArtistRepo;
    use crate::db::models::{Artist, Track};
    use crate::db::track_repo::TrackRepo;

    let db = pg_or_skip!();
    reset_schema(&db);
    let artist_repo = ArtistRepo::with_backend(db.clone());

    let first = artist_repo
        .get_or_create("Classique - Saint-Saëns", Some(""), None)
        .unwrap();
    let second = artist_repo
        .get_or_create("Anouar Brahem", Some(""), None)
        .unwrap();
    assert_ne!(first.id, second.id, "un MBID vide ne doit pas être partagé");

    let wrong = artist_repo
        .create(&Artist::new("Ancien artiste collé".into()))
        .unwrap();
    let right = artist_repo
        .create(&Artist::new("Artiste unanime des pistes".into()))
        .unwrap();
    db.execute(
        "UPDATE artists SET musicbrainz_id = '' WHERE id = $1",
        &[&wrong],
    )
    .unwrap();

    let album_repo = AlbumRepo::with_backend(db.clone());
    let album = album_repo
        .get_or_create_for_folder("/music/pg2458", "PG 2458", wrong, None, None)
        .unwrap();
    let album_id = album.id.unwrap();
    let track_repo = TrackRepo::with_backend(db.clone());
    for number in 1..=2 {
        let mut track = Track::new(format!("Piste {number}"));
        track.album_id = Some(album_id);
        track.artist_id = Some(right);
        track.track_number = number;
        track.file_path = Some(format!("/music/pg2458/{number:02}.flac"));
        track_repo.create(&track).unwrap();
    }

    assert_eq!(album_repo.repair_empty_mbid_artist_collapses().unwrap(), 1);
    assert_eq!(
        album_repo.get(album_id).unwrap().unwrap().artist_id,
        Some(right)
    );
    reset_schema(&db);
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
        context_type: None,
        context_id: None,
        context_position: None,
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

/// Regression for forum #1220 (tester Jean-François, PostgreSQL backend): a
/// SQLite→PG data-migrated database had its numeric columns created as TEXT,
/// so the force-scan album lookup `... WHERE year = $int` threw
/// `operator does not exist: text = bigint` and EVERY album write failed
/// (22841 failures, +0 added). The heal chain (010 albums/tracks, 011
/// listen_history, 013 the rest) converts those columns back to their numeric
/// types at startup. This test asserts the post-migration schema is numeric and
/// that the exact failing query pattern now runs cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn pg_1220_numeric_columns_have_numeric_types() {
    let db = pg_or_skip!();

    // (table, column, acceptable PG data_type). Columns from later migrations
    // may be absent on a partial schema — such rows are skipped, not failed.
    let expected: &[(&str, &str, &[&str])] = &[
        // 010 (albums/tracks)
        ("albums", "year", &["integer"]),
        ("albums", "disc_count", &["integer"]),
        ("albums", "sample_rate", &["integer"]),
        ("tracks", "duration_ms", &["bigint"]),
        ("tracks", "track_number", &["integer"]),
        ("tracks", "bpm", &["double precision"]),
        // 011 (listen_history)
        ("listen_history", "duration_ms", &["bigint"]),
        // 013 (the rest)
        ("zones", "volume", &["integer"]),
        ("zones", "last_position_ms", &["bigint"]),
        ("queue_items", "position", &["integer"]),
        ("track_source_links", "confidence", &["double precision"]),
        ("bookmarks", "position_ms", &["bigint"]),
    ];

    for (table, col, ok_types) in expected {
        let t = table.to_string();
        let cc = col.to_string();
        let row = db
            .query_one(
                "SELECT data_type FROM information_schema.columns \
                 WHERE table_name = $1 AND column_name = $2",
                &[&t, &cc],
            )
            .unwrap();
        let Some(cols) = row else {
            continue; // table/column not present in this schema — skip
        };
        let dt = cols[0].as_str().unwrap_or("").to_string();
        assert!(
            ok_types.contains(&dt.as_str()),
            "{table}.{col} is `{dt}`, expected one of {ok_types:?} — heal migration missing/incomplete"
        );
    }

    // The exact #1220 failing pattern: `year` bound as an integer parameter.
    // On a TEXT column this raised `operator does not exist: text = bigint`;
    // after the heal it must execute without error.
    db.query_many("SELECT id FROM albums WHERE year = $1 LIMIT 1", &[&2020i32])
        .expect("WHERE year = $int must not raise `text = bigint` after the heal");
}

/// #2468 — contre le chemin reel d'une base deja installee : 005 a cree
/// `bookmarks.position_ms` en INTEGER et 013 a enregistre son passage sans la
/// toucher. La migration suivante doit etre jouee par le runner du binaire,
/// convertir sans perte, puis permettre une position superieure a i32::MAX.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2468_runner_heals_bookmarks_position_integer_to_bigint() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();

    sqlx::raw_sql(
        "DELETE FROM bookmarks;
         ALTER TABLE bookmarks
             ALTER COLUMN position_ms TYPE INTEGER
             USING position_ms::integer;
         DELETE FROM schema_version WHERE version = 36;",
    )
    .execute(&pool)
    .await
    .expect("le drift INTEGER de #2468 doit pouvoir etre reproduit");

    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .expect("le runner doit appliquer la migration 036");

    let data_type: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
          WHERE table_schema = current_schema()
            AND table_name = 'bookmarks'
            AND column_name = 'position_ms'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(data_type, "bigint");

    let large_position = i64::from(i32::MAX) + 1;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO bookmarks (position_ms, label)
         VALUES ($1, 'pg-2468-i64')
         RETURNING id",
    )
    .bind(large_position)
    .fetch_one(&pool)
    .await
    .expect("bookmarks.position_ms doit accepter toute valeur i64");
    let stored: i64 = sqlx::query_scalar("SELECT position_ms FROM bookmarks WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, large_position);
    sqlx::query("DELETE FROM bookmarks WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
}

/// #1706 — reproduces the exact .15 production drift and proves `ensure_schema`
/// heals it instead of dying on the first bad statement.
///
/// The drift: `streaming_favorites.id` is BIGINT (migration 012 converts the
/// TEXT ids of a SQLite→PG migrated database back to bigint + sequence), while
/// `ensure_schema` re-imposed a `nextval(...)::text` DEFAULT on it. Because the
/// whole self-healing DDL went out as ONE multi-statement query — one implicit
/// transaction — that single failure discarded everything, including the
/// `CREATE TABLE queue_items`. And that CREATE was itself missing
/// track_number/disc_number, which every streaming queue write names.
/// Net effect on .15: `queue_restore_append_failed` for 9 zones, every boot.
#[tokio::test(flavor = "multi_thread")]
async fn pg_1706_ensure_schema_heals_queue_items_numbering() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();

    // Rebuild the broken pre-fix state.
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS queue_items CASCADE;
         DROP TABLE IF EXISTS streaming_favorites CASCADE;
         DROP SEQUENCE IF EXISTS streaming_favorites_id_seq;
         CREATE TABLE streaming_favorites (
             id BIGINT PRIMARY KEY,
             profile_id TEXT NOT NULL DEFAULT '1',
             item_type TEXT NOT NULL,
             service TEXT NOT NULL,
             service_id TEXT NOT NULL,
             title TEXT,
             artist TEXT,
             album TEXT,
             cover_url TEXT,
             created_at TEXT,
             UNIQUE(profile_id, item_type, service, service_id)
         );",
    )
    .execute(&pool)
    .await
    .expect("seeding the drifted schema must succeed");

    // Boot the backend: connect() runs ensure_schema().
    let db = crate::db::postgres::PostgresDb::connect(&url)
        .await
        .expect("connect must succeed");

    // The statement that used to abort the batch is now guarded, so everything
    // AFTER it ran: queue_items exists…
    let exists: Option<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_name = 'queue_items'",
    )
    .fetch_optional(db.pool())
    .await
    .unwrap();
    assert!(
        exists.is_some(),
        "queue_items was not created: a failing statement still rolls back the batch"
    );

    // …and it carries the numbering columns, as BIGINT (bound as i64).
    for col in ["track_number", "disc_number"] {
        let dt: Option<String> = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'queue_items' AND column_name = $1",
        )
        .bind(col)
        .fetch_optional(db.pool())
        .await
        .unwrap();
        assert_eq!(
            dt.as_deref(),
            Some("bigint"),
            "queue_items.{col} missing or not bigint — streaming queue writes will fail"
        );
    }

    // The failing write from the ticket, verbatim in shape: a streaming row
    // naming track_number/disc_number must now insert.
    sqlx::raw_sql(
        "INSERT INTO queue_items \
         (zone_id, position, source_id, title, artist, album, cover_url, duration_ms, source, track_number, disc_number) \
         VALUES (424242, 0, 'q1', 't', 'a', 'al', NULL, 1000, 'qobuz', 3, 1)",
    )
    .execute(db.pool())
    .await
    .expect("streaming queue insert must succeed once the numbering columns exist");

    // Migration 026 is what repairs an ALREADY installed database: the seeded
    // `streaming_favorites.id` is BIGINT with no DEFAULT at all (the guarded
    // ALTER deliberately leaves a non-text column alone), so an insert that
    // omits `id` — which is what the repo does — fails until 026 re-attaches an
    // integer sequence. Replaying it here also asserts its idempotence: the CI
    // database has already had it applied by the migration step.
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/026_queue_items_numbering.sql"
    ))
    .execute(db.pool())
    .await
    .expect("migration 026 must be replayable");

    sqlx::raw_sql(
        "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id) \
         VALUES ('1', 'album', 'qobuz', 'a1')",
    )
    .execute(db.pool())
    .await
    .expect("streaming_favorites must stay insertable without an explicit id");

    // Leave the schema in the shape the other tests expect.
    sqlx::raw_sql("DELETE FROM queue_items WHERE zone_id = 424242")
        .execute(db.pool())
        .await
        .ok();
}
