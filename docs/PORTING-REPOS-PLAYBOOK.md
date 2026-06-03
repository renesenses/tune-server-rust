# Porting repos to `Arc<dyn DbBackend>`

PG roadmap phase 5 — mechanical refactor of the 13 repos so each can
run on either SQLite or Postgres at runtime.

`settings_repo` is the reference implementation (cf. commit landing this
playbook). The same 6 steps apply to every other repo.

---

## Inventory

| Repo | Reads | Writes | LOC | Difficulty |
|---|---:|---:|---:|---|
| `source_link_repo` | 5 | 1 | 194 | trivial |
| `settings_repo` | 5 | 1 | 215 | trivial — **done** |
| `rating_repo` | 5 | 2 | 309 | trivial |
| `tag_repo` | 14 | 5 | 408 | easy |
| `profile_repo` | 10 | 5 | 434 | easy |
| `playlist_repo` | 14 | 5 | 510 | easy |
| `radio_repo` | 14 | 4 | 514 | easy |
| `zone_repo` | 12 | 14 | 545 | medium (PG quirks possible — has WAL fallback) |
| `artist_repo` | 22 | 4 | 562 | medium (FTS rebuild paths) |
| `play_queue_repo` | 17 | 0 | 662 | medium (parallel-agent area) |
| `history_repo` | 46 | 1 | 952 | hard (heavy aggregation) |
| `album_repo` | 50 | 8 | 1307 | hard (joins, FTS, filters) |
| `track_repo` | 65 | 14 | 1335 | hard (the big one — touch last) |

Total ~290 read sites. Each repo is independent and can be ported in
isolation; tests are the safety net.

---

## The 6-step pattern (per repo)

### 1. Storage

```rust
// Before
pub struct FooRepo {
    db: SqliteDb,
}

// After
pub struct FooRepo {
    db: Arc<dyn DbBackend>,
}
```

### 2. Constructors

Keep `new(db: SqliteDb)` for backward compatibility (every call site
already passes `SqliteDb`), and add `with_backend(Arc<dyn DbBackend>)`
for PG-driven code paths.

```rust
impl FooRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }
}
```

This means **zero call-site changes** — the 271 `SettingsRepo::new(db)`,
all the `TrackRepo::new(db)`, etc., keep compiling.

### 3. SQL builders

If the repo's `sql::*` module isn't already engine-agnostic, convert
each builder to take a `&D: SqlDialect` and emit placeholders via
`d.placeholder(n)`.

```rust
// Before
pub fn get_by_id() -> &'static str { "SELECT … WHERE id = ?" }

// After
pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
    format!("SELECT … WHERE id = {}", d.placeholder(1))
}
```

Add unit tests that assert the SQLite output (`?`) and the Postgres
output (`$1`) both round-trip correctly — `settings_repo` has the
template.

### 4. Method bodies

Replace direct rusqlite calls with `DbBackend` trait calls:

| Before (rusqlite-direct) | After (`DbBackend`) |
|---|---|
| `let conn = self.db.read_connection().lock().unwrap(); conn.query_row(sql, params, mapper).optional()` | `self.db.query_one(&sql, &params)?.map(decode)` |
| `let mut stmt = conn.prepare(sql)?; let rows = stmt.query_map(...)?.collect()` | `self.db.query_many(&sql, &params)?.into_iter().map(decode).collect()` |
| `self.db.execute(sql, &rusqlite_params)` | `self.db.execute(&sql, &dbbackend_params)` |
| `self.db.last_insert_rowid()` | unchanged (already on the trait) |
| `self.db.read(\|conn\| ...)` / `self.db.write(\|conn\| ...)` | rewrite with `query_one` / `query_many` / `execute` |
| `self.db.query_timed(label, \|c\| …)` | drop the timing wrapper, or add it later as a higher-level decorator |

For each query, build the SQL via `self.dialect_sql(sql::builder, sql::builder)`
(see `settings_repo::dialect_sql`) so the same code path serves both engines.

### 5. Row decoding

Replace the rusqlite `|row| row.get::<_, T>(i)` closures with the
`SqlValue` helpers:

| rusqlite | `SqlValue` |
|---|---|
| `row.get::<_, i64>(0)` | `cols[0].as_i64().ok_or(...)?` |
| `row.get::<_, Option<String>>(1)` | `cols[1].as_string()` (already optional) |
| `row.get::<_, f64>(2)` | `cols[2].as_f64().ok_or(...)?` |
| `row.get::<_, bool>(3)` | `cols[3].as_bool().ok_or(...)?` |
| `row.get::<_, Vec<u8>>(4)` | `cols[4].as_blob().map(<[u8]>::to_vec)` |

A common pattern: define a private `fn row_to_foo(cols: &[SqlValue]) -> Result<Foo, String>`
that maps positionally. Then `query_one` and `query_many` both call it.

### 6. Tests

The existing tests should keep passing without modification (because
`new(SqliteDb)` is preserved). Add **one new test** per repo that uses
`with_backend(Arc<dyn DbBackend>)` to prove the trait-object path works
— `settings_repo::with_backend_constructor` is the template.

---

## Workflow

For each repo:

```bash
git switch -c port/<repo>-dbbackend origin/main
# Edit the repo file using the 6 steps above
cargo test -p tune-core <repo>::tests:: --no-fail-fast
cargo check -p tune-core
cargo check -p tune-server  # ensures call-sites still work
git commit -m "feat(db): port <repo> to Arc<dyn DbBackend>"
git push origin HEAD:main
```

Each port is **one focused commit**. No batching — keeps blast radius
minimal and any regression bisects cleanly to a single repo.

---

## Things to watch for

- **`read_connection` vs `connection`**: rusqlite repos use the read
  conn for SELECTs and the write conn for the WAL read-lag fallback
  (cf. `8af95ec`). The new abstraction always reads from the write
  pool in PG (no WAL), so behavior matches.
- **`last_insert_rowid` semantics**: identical between engines (PG impl
  uses `RETURNING id` internally).
- **JSON columns**: SQLite uses `TEXT`, PG uses `JSONB`. Builder SQL
  needs to emit `::jsonb` casts on PG side for INSERTs (the dialect
  trait doesn't yet handle this — add a `json_cast()` method when the
  first JSON-heavy repo lands, currently `track_repo.genres`).
- **FTS5 vs tsvector**: `engine.rs::fts_where` already handles this. The
  repo just calls `dialect.fts_where(...)` — no per-engine branching
  needed in the repo.
- **In-memory tests**: `SqliteDb::open_in_memory()` returns the same
  connection for read+write. PG has no in-memory; tests stay SQLite-only,
  and the PG path is tested via the E2E test gated on
  `TUNE_TEST_PG_URL`.

---

## Order of attack

Suggested order — easiest first, build confidence, hardest last:

1. ~~`settings_repo`~~ ✅ (POC, fe66002)
2. ~~`source_link_repo`~~ ✅ (145ceb3)
3. ~~`rating_repo`~~ ✅ (145ceb3)
4. ~~`tag_repo`~~ ✅ (ea1f1aa)
5. ~~`profile_repo`~~ ✅ (ea1f1aa)
6. ~~`radio_repo`~~ ✅ (2e915ad)
7. ~~`playlist_repo`~~ ✅ (this commit — first tx user via `write_tx`)
8. `zone_repo` — 6 tx sites, has WAL fallback to preserve
9. `artist_repo` — 2 tx sites (FTS rebuild paths)
10. `play_queue_repo` — 12 tx sites, parallel-agent territory
11. `history_repo` — 952 LOC, `full_dashboard` is a 300-line method
    building 10 sub-queries. Needs the date helpers (✅ landed) plus
    careful systematic rewrite of the inline format strings into
    dialect-driven builders. Plan as a dedicated session.
12. `album_repo`, `track_repo` — the big ones (1307 / 1335 LOC),
    plenty of FTS + joins + complex filters. Dedicated session each.

Each step is independent. Stop after any step and the workspace still
compiles + runs on SQLite. Postgres production rollout waits for the
last repo.

---

## Transaction abstraction (next infra commit)

Six of the remaining repos use `self.db.connection().lock()` then
`conn.transaction()` for atomic multi-statement writes (e.g.
`playlist_repo::add_tracks` does max_pos query + N inserts inside a
single tx). The current trait can't express that.

Proposed trait extension:

```rust
pub trait DbBackend: Send + Sync {
    // ... existing methods ...

    /// Run a closure inside a write transaction. Commits on Ok return,
    /// rolls back on Err or panic. The handle exposes the same trait
    /// surface as DbBackend for write/read operations.
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String>;
}

pub trait DbTxHandle: Send + Sync {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String>;
    fn query_one(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<Option<Vec<SqlValue>>, String>;
    fn query_many(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String>;
    fn last_insert_rowid(&self) -> i64;
}
```

Implementation notes:
- SQLite: lock the write conn, `conn.transaction()`, wrap in a
  `SqliteTxHandle { tx: &rusqlite::Transaction }` that forwards
  `execute` / `query_*` to `tx.execute` / `tx.prepare`.
- Postgres: `pool.begin().await` returns a `sqlx::Transaction<'_, Postgres>`,
  wrap in a `PostgresTxHandle` that runs through `block_in_place`
  similarly to `PostgresBackend::execute`.
- Repos that need a tx swap from `self.db.connection().lock().unwrap()`
  to `self.db.write_tx(&mut |tx| { ... })`. The methods inside use
  the tx handle's trait surface, which has the same SQL signatures as
  DbBackend.

Estimated effort for the trait + 2 impls: ~3-4h. Then porting each of
the 6 remaining transactional repos becomes mechanical (same pattern
as the first 6, just via tx instead of self.db).
