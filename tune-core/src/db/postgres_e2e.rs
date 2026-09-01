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
    assert_eq!(z.volume, 50.0);

    repo.update_volume(id, 75.0).unwrap();
    assert_eq!(repo.get(id).unwrap().unwrap().volume, 75.0);
    // #2886 — la colonne est a virgule des DEUX cotes. Sur PG c'est la
    // migration 048 qui le garantit : sans elle, ecrire un f64 dans une
    // colonne `integer` echoue purement et simplement.
    repo.update_volume(id, 0.398_107_170_553_497_2 * 100.0)
        .unwrap();
    let relu = repo.get(id).unwrap().unwrap().volume / 100.0;
    assert!(
        (relu - 0.398_107_170_553_497_2).abs() < 1e-12,
        "-8 dB persiste puis relu a {relu}"
    );
    repo.update_volume(id, 10f64.powf(-48.0 / 20.0) * 100.0)
        .unwrap();
    assert!(
        repo.get(id).unwrap().unwrap().volume > 0.0,
        "-48 dB : la zone se rallumerait MUETTE sur PostgreSQL"
    );
    repo.update_volume(id, 75.0).unwrap();

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
        // #2886 — a virgule : l'entier coupait le son sous -46,02 dB.
        ("zones", "volume", &["double precision"]),
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

/// #2860 — « Continuer l'écoute » et « Ajoutés récemment » étaient vides sur
/// TOUTE installation PostgreSQL, et sans un seul message.
///
/// Les trois défauts, mesurés sur une base réelle. Chacun porte ici sa
/// contre-épreuve : on rejoue la forme d'AVANT et on exige l'erreur exacte,
/// pour qu'un retour en arrière ne puisse pas passer inaperçu.
///
/// 1. `listen_history.album_id` TEXT contre `albums.id` BIGINT —
///    `operator does not exist: text = bigint`. La migration 012 convertit
///    déjà cette colonne, mais elle ne l'a JAMAIS vue : `album_id` n'arrive
///    par aucun script numéroté, seulement par `ENSURE_COLUMNS`, rejoué APRÈS.
///    Réparé par la 047.
/// 2. `GROUP BY a.id` en sélectionnant `ar.name` —
///    `column "ar.name" must appear in the GROUP BY clause…`.
/// 3. `HAVING listened_tracks < …` — un alias de la liste SELECT n'existe pas
///    quand le HAVING est évalué : `column "listened_tracks" does not exist`.
///
/// Les trois erreurs étaient avalées par le `unwrap_or_default()` de
/// `tune-server/src/routes/home.rs` : la section ne s'expliquait pas, elle
/// disparaissait.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2860_continuer_lecoute_et_ajouts_recents() {
    use crate::db::backend::ToSqlValue;
    use crate::db::engine::Engine;
    use crate::db::home_queries::{continue_listening_albums_deduits, recently_added};
    use crate::db::migrations::PG_MIGRATIONS;

    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL not set, skipping PG E2E test");
        return;
    };
    // Le pool brut EN PLUS du backend : `execute_batch` decoupe sur les
    // point-virgules, ce qui hacherait le bloc `DO $migration$ … $migration$`
    // de la 047. Les scripts numerotes passent par `raw_sql`, comme le fait
    // deja le test #1706.
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pool.clone()));
    reset_schema(&db);

    let type_album_id = |db: &Arc<dyn DbBackend>| -> String {
        db.query_many(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'listen_history' AND column_name = 'album_id'",
            &[],
        )
        .unwrap()
        .first()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .unwrap_or_else(|| "<absente>".into())
    };

    // ── Reconstituer la dérive : la colonne telle qu'ENSURE_COLUMNS la posait ──
    db.execute(
        "ALTER TABLE listen_history DROP COLUMN IF EXISTS album_id",
        &[],
    )
    .unwrap();
    db.execute("ALTER TABLE listen_history ADD COLUMN album_id TEXT", &[])
        .unwrap();
    assert_eq!(type_album_id(&db), "text", "la dérive n'a pas été reposée");

    let limite: i64 = 10;
    let sql_cl = continue_listening_albums_deduits(Engine::Postgres, "");
    let sql_ra = recently_added(Engine::Postgres);

    // ── CONTRE-ÉPREUVE 1 — sans la migration 047, la requête ne compile même pas.
    let err = db
        .query_many(&sql_cl, &[&limite as &dyn ToSqlValue])
        .expect_err("`text = bigint` doit être refusé par PostgreSQL");
    assert!(
        err.contains("operator does not exist") && err.contains("text") && err.contains("bigint"),
        "erreur attendue « operator does not exist: text = bigint », obtenue : {err}"
    );

    // ── La migration 047, rejouée telle quelle (elle est idempotente) ──
    let (_, _, sql_047) = PG_MIGRATIONS
        .iter()
        .find(|(v, _, _)| *v == 47)
        .expect("la migration 047 doit être inscrite dans PG_MIGRATIONS");
    sqlx::raw_sql(*sql_047)
        .execute(&pool)
        .await
        .expect("la migration 047 doit être rejouable");
    assert_eq!(
        type_album_id(&db),
        "bigint",
        "la 047 n'a pas converti listen_history.album_id"
    );

    // ── Les données : les deux « Live » de Tades, dont un seul est écouté ──
    let id_de = |sql: &str| -> i64 {
        db.query_many(sql, &[])
            .unwrap()
            .first()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    };
    let police = id_de("INSERT INTO artists (name) VALUES ('The Police') RETURNING id");
    let pulp = id_de("INSERT INTO artists (name) VALUES ('Pulp') RETURNING id");
    let live_police = id_de(&format!(
        "INSERT INTO albums (title, artist_id, track_count) \
         VALUES ('Live', {police}, 5) RETURNING id"
    ));
    let live_pulp = id_de(&format!(
        "INSERT INTO albums (title, artist_id, track_count) \
         VALUES ('Live', {pulp}, 5) RETURNING id"
    ));
    for piste in ["Piste 1", "Piste 2"] {
        db.execute(
            &format!(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, listened_at) \
                 VALUES ('{piste}', 'Pulp', 'Live', {live_pulp}, '2026-08-28T22:45:00Z')"
            ),
            &[],
        )
        .unwrap();
    }

    // ── « Continuer l'écoute » rend le disque de Pulp, 2 pistes sur 5 ──
    let lignes = db
        .query_many(&sql_cl, &[&limite as &dyn ToSqlValue])
        .expect("la requête corrigée doit s'exécuter sur PostgreSQL");
    assert_eq!(
        lignes.len(),
        1,
        "un seul album écouté et non fini était attendu, obtenu : {lignes:?}"
    );
    let l = &lignes[0];
    assert_eq!(
        l[0].as_i64(),
        Some(live_pulp),
        "ce n'est pas le Live de Pulp"
    );
    assert_ne!(
        l[0].as_i64(),
        Some(live_police),
        "l'homonyme de Police est remonté (#2731)"
    );
    assert_eq!(l[1].as_string().as_deref(), Some("Live"));
    assert_eq!(
        l[2].as_string().as_deref(),
        Some("Pulp"),
        "`ar.name` doit être rendue — c'est la colonne qui faisait tomber la requête"
    );
    assert_eq!(l[6].as_i64(), Some(2), "2 pistes distinctes écoutées");
    assert_eq!(l[7].as_i64(), Some(5), "sur 5");

    // ── « Ajoutés récemment » : même défaut, même écran ──
    let piste = id_de(&format!(
        "INSERT INTO tracks (title, album_id, artist_id, file_path, file_mtime) \
         VALUES ('Piste 1', {live_pulp}, {pulp}, '/x/1.flac', 9999999999) RETURNING id"
    ));
    assert!(piste > 0);
    let depuis: i64 = 0;
    let recents = db
        .query_many(
            &sql_ra,
            &[&depuis as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
        )
        .expect("« Ajoutés récemment » doit s'exécuter sur PostgreSQL");
    assert_eq!(recents.len(), 1, "obtenu : {recents:?}");
    assert_eq!(recents[0][2].as_string().as_deref(), Some("Pulp"));

    // ── CONTRE-ÉPREUVE 2 — `GROUP BY a.id` seul, la forme d'avant ──
    let avant_group_by = sql_cl.replace(
        "GROUP BY a.id, a.title, ar.name, a.year, a.cover_path, a.genre, a.track_count",
        "GROUP BY a.id",
    );
    assert_ne!(avant_group_by, sql_cl, "la substitution n'a rien remplacé");
    let err = db
        .query_many(&avant_group_by, &[&limite as &dyn ToSqlValue])
        .expect_err("`GROUP BY a.id` avec `ar.name` doit être refusé");
    assert!(
        err.contains("ar.name") && err.contains("GROUP BY"),
        "erreur attendue sur « ar.name », obtenue : {err}"
    );

    // ── CONTRE-ÉPREUVE 3 — l'alias de la liste SELECT dans le HAVING ──
    let avant_having = sql_cl.replace(
        "HAVING COUNT(DISTINCT lh.title) < a.track_count",
        "HAVING listened_tracks < a.track_count",
    );
    assert_ne!(avant_having, sql_cl, "la substitution n'a rien remplacé");
    let err = db
        .query_many(&avant_having, &[&limite as &dyn ToSqlValue])
        .expect_err("un alias de la liste SELECT dans le HAVING doit être refusé");
    assert!(
        err.contains("listened_tracks") && err.contains("does not exist"),
        "erreur attendue « column \"listened_tracks\" does not exist », obtenue : {err}"
    );
}

/// #2441 — « Continuer l'ecoute » sur une VRAIE base PostgreSQL : les
/// contextes, leur ordre, et l'avancement que le client dessine.
///
/// # Pourquoi ce test manquait
///
/// Le correctif de #2441 (PR #2479 puis #2936) a mis « Continuer l'ecoute » a
/// partir de `listen_history` et de son contexte de lecture. Les DEUX requetes
/// qui le portent — la derniere ecoute de chaque contexte, et la resolution
/// des albums locaux avec leur avancement — etaient redigees dans
/// `tune-server/src/routes/home.rs`. Or ce job lance `cargo test -p tune-core`
/// et ne compile PAS `tune-server` : elles n'avaient donc jamais ete jouees
/// sur PostgreSQL, exactement comme les requetes de #2860 avant elles, et pour
/// la meme raison. Leurs erreurs seraient avalees par le
/// `unwrap_or_default()` de l'appelant : pas un message, juste une section
/// vide.
///
/// Elles sont descendues dans `db/home_queries.rs`, et ce test les EXECUTE.
///
/// # Ce qu'il etablit
///
/// 1. Les deux requetes s'executent sur PostgreSQL.
/// 2. Un historique couvrant TROIS albums en rend trois, du plus recent au
///    plus ancien, sans doublon — le fait de base du ticket.
/// 3. `progression_pourcent` rend 60 / 40 / 20 — **les memes nombres** que le
///    test SQLite `plusieurs_albums_entames_rendent_chacun_leur_avancement`
///    (tune-server/src/routes/home.rs). C'est la comparaison des deux moteurs.
/// 4. TEMOIN : le cas a un seul album rend exactement cet album.
#[tokio::test(flavor = "multi_thread")]
async fn pg_2441_continuer_lecoute_contextes_et_progression() {
    use crate::db::backend::ToSqlValue;
    use crate::db::engine::Engine;
    use crate::db::home_queries::{
        continue_listening_albums_du_contexte, continue_listening_contextes, progression_pourcent,
    };

    let db = pg_or_skip!();
    reset_schema(&db);

    let id_de = |sql: &str| -> i64 {
        db.query_many(sql, &[])
            .unwrap()
            .first()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    };

    // ── Trois disques de cinq pistes, entames de 1, 2 et 3 pistes ──
    let mut albums = Vec::new();
    for (rang, nom) in ["Un", "Deux", "Trois"].iter().enumerate() {
        let artiste = id_de(&format!(
            "INSERT INTO artists (name) VALUES ('Artiste {nom}') RETURNING id"
        ));
        let album = id_de(&format!(
            "INSERT INTO albums (title, artist_id, track_count) \
             VALUES ('Disque {nom}', {artiste}, 5) RETURNING id"
        ));
        // Le plus ANCIEN est le moins ecoute : l'ordre attendu est celui de
        // l'ecoute, pas celui de l'avancement.
        for piste in 0..=rang {
            db.execute(
                &format!(
                    "INSERT INTO listen_history \
                     (title, artist_name, album_title, album_id, source, \
                      context_type, context_id, listened_at) \
                     VALUES ('{nom}{piste}', 'Artiste {nom}', 'Disque {nom}', \
                             {album}, 'local', 'album', '{album}', \
                             '2026-08-2{rang}T10:0{piste}:00Z')"
                ),
                &[],
            )
            .unwrap();
        }
        albums.push(album);
    }
    let (un, deux, trois) = (albums[0], albums[1], albums[2]);

    // ── 1. La requete des contextes s'execute, et rend les TROIS ──
    let marge: i64 = 40;
    let sql_ctx = continue_listening_contextes(Engine::Postgres, "");
    let lignes = db
        .query_many(&sql_ctx, &[&marge as &dyn ToSqlValue])
        .expect("`continue_listening_contextes` doit s'executer sur PostgreSQL");

    let contextes: Vec<(String, String)> = lignes
        .iter()
        .filter_map(|c| {
            Some((
                c.first().and_then(|v| v.as_string())?,
                c.get(1).and_then(|v| v.as_string())?,
            ))
        })
        .collect();
    assert_eq!(
        contextes.len(),
        3,
        "les trois contextes album etaient attendus, obtenu : {contextes:?}"
    );

    // 2. Le bon ordre — du plus recent au plus ancien — et sans doublon.
    let ids: Vec<String> = contextes.iter().map(|(_, id)| id.clone()).collect();
    assert_eq!(
        ids,
        vec![trois.to_string(), deux.to_string(), un.to_string()],
        "l'ordre doit etre celui de la derniere ecoute"
    );
    let uniques: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(ids.len(), uniques.len(), "un contexte remonte deux fois");
    assert!(
        contextes.iter().all(|(nature, _)| nature == "album"),
        "toutes les entrees sont de nature album : {contextes:?}"
    );

    // ── 3. L'avancement : les memes nombres que sur SQLite ──
    let sql_alb = continue_listening_albums_du_contexte(&[un, deux, trois]);
    let resolus = db
        .query_many(&sql_alb, &[])
        .expect("`continue_listening_albums_du_contexte` doit s'executer sur PostgreSQL");

    let mut pourcents = std::collections::HashMap::new();
    for cols in &resolus {
        let id = cols.first().and_then(|v| v.as_i64()).unwrap();
        let ecoutees = cols.get(6).and_then(|v| v.as_i64());
        let total = cols.get(7).and_then(|v| v.as_i64());
        pourcents.insert(id, progression_pourcent(ecoutees, total));
    }
    assert_eq!(
        (
            pourcents.get(&trois).copied().flatten(),
            pourcents.get(&deux).copied().flatten(),
            pourcents.get(&un).copied().flatten()
        ),
        (Some(60), Some(40), Some(20)),
        "PostgreSQL doit rendre le MEME avancement que SQLite (3/5, 2/5, 1/5), \
         obtenu : {pourcents:?}"
    );

    // ── 4. TEMOIN — un seul album rend exactement cet album ──
    reset_schema(&db);
    let artiste = id_de("INSERT INTO artists (name) VALUES ('Pulp') RETURNING id");
    let seul = id_de(&format!(
        "INSERT INTO albums (title, artist_id, track_count) \
         VALUES ('Live', {artiste}, 5) RETURNING id"
    ));
    for piste in ["Common People", "Disco 2000"] {
        db.execute(
            &format!(
                "INSERT INTO listen_history \
                 (title, artist_name, album_title, album_id, source, \
                  context_type, context_id, listened_at) \
                 VALUES ('{piste}', 'Pulp', 'Live', {seul}, 'local', \
                         'album', '{seul}', '2026-08-28T22:45:00Z')"
            ),
            &[],
        )
        .unwrap();
    }

    let lignes = db
        .query_many(&sql_ctx, &[&marge as &dyn ToSqlValue])
        .expect("la requete des contextes doit s'executer");

    // Les deux pistes portent la MEME `listened_at` — a la seconde pres, ce
    // qu'un enchainement produit — et la jointure sur le MAX les rend donc
    // TOUTES LES DEUX. Mesure du 01/09 sur PostgreSQL 15 : la requete rend
    // bien deux lignes ici. Le dedoublonnage est en Rust, chez l'appelant
    // (`contextes_recents`, tune-server/src/routes/home.rs) qui garde la
    // premiere, l'ordre etant deja decroissant. On rejoue cette regle pour
    // verifier le contrat REEL de la requete, pas un contrat imagine.
    let mut vues = std::collections::HashSet::new();
    let distincts: Vec<String> = lignes
        .iter()
        .filter_map(|c| {
            let nature = c.first().and_then(|v| v.as_string())?;
            let id = c.get(1).and_then(|v| v.as_string())?;
            vues.insert((nature, id.clone())).then_some(id)
        })
        .collect();
    assert_eq!(
        distincts,
        vec![seul.to_string()],
        "le temoin doit rendre exactement l'album ecoute : {lignes:?}"
    );

    let resolus = db
        .query_many(&continue_listening_albums_du_contexte(&[seul]), &[])
        .expect("la resolution d'album doit s'executer");
    assert_eq!(resolus.len(), 1);
    assert_eq!(
        progression_pourcent(
            resolus[0].get(6).and_then(|v| v.as_i64()),
            resolus[0].get(7).and_then(|v| v.as_i64())
        ),
        Some(40),
        "2 pistes sur 5, comme sur SQLite : {resolus:?}"
    );
}
