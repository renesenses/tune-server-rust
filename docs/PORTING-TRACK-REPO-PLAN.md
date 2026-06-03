# Porting `track_repo` to `Arc<dyn DbBackend>` — detailed plan

`track_repo.rs` is the largest of the 13 repos:
- **1335 LOC** (55 public methods)
- **65 read sites**, **14 write sites**, 5 explicit transactions
- A `pub fn db(&self) -> SqliteDb` getter that callers use to grab
  the SQLite handle directly — needs special handling
- Many methods with **inline SQL** that bypass the `sql::*` builders
  (set_synced_lyrics, get_trailing_silence, set_acoustid,
  list_unidentified, get_credits, get_all_paths,
  get_all_local_file_info, get_existing_audio_hash_album_pairs, etc.)
- Complex return types: `HashSet<String>`, `HashMap<String, (i64,
  Option<f64>, Option<i64>)>`, `HashSet<(String, i64)>`
- 2 batch methods (`create_batch`, `update_batch`) that hold the conn
  across N statements — needs `write_tx` rewrite for atomicity
- Several methods with SQLite-specific features: `RANDOM()`,
  `LIKE 'Track %'` heuristics, `synced_lyrics` / `waveform_json` /
  `trailing_silence_ms` / `acoustid_*` columns

Estimate: **4-6 hours focused work** (a full session). Do not bundle
with other work.

---

## Pre-flight

Required infra (all ✅ at the time of writing):

- [x] `DbBackend::query_one` / `query_many` / `query_many_strong` /
  `execute` / `last_insert_rowid` / `write_tx`
- [x] `SqlValue` decode helpers
- [x] `SqlDialect` date helpers, `placeholder`, `fts_where`

No new infra needed. The work is mechanical method-by-method porting.

---

## Method-by-method plan

### Group A — pure mechanical (~1h, 20 methods)

These already have dialect-aware `sql::*` builders. Port body to
`self.db.query_one` / `query_many` / `execute`:

| Method | LOC |
|---|---:|
| `get`, `get_by_path`, `count`, `delete` | 4 × ~5 |
| `create`, `update` | 2 × ~40 |
| `delete_by_path` | ~8 |
| `delete_all` | ~10 |
| `list`, `list_paginated`, `list_by_album`, `list_by_artist` | 4 × ~15 |
| `update_mtime_and_size`, `update_audio_hash` | 2 × ~10 |
| `search`, `search_by_title`, `find_by_path` | 3 × ~20 |

### Group B — inline SQL → builders (~1h, ~10 methods)

These currently embed SQL inline. Promote to `sql::*` builders then
port:

| Method | What needs a builder |
|---|---|
| `set_synced_lyrics` / `get_synced_lyrics` | `update_field` / `select_field` for `synced_lyrics` |
| `set_trailing_silence` / `get_trailing_silence` | same pattern, `trailing_silence_ms` |
| `set_waveform` / `get_waveform` | same pattern, `waveform_json` |
| `set_acoustid` | 2-field UPDATE |
| `list_unidentified` | heuristic WHERE clause — needs a builder + LIMIT |
| `get_credits` | track_credits SELECT — add a `select_credits` builder |
| `get_all_paths` | simple SELECT — builder |
| `get_all_local_file_info` | `(id, file_path, file_mtime, file_size)` SELECT |
| `get_existing_audio_hash_album_pairs` | DISTINCT SELECT |
| `exists_by_audio_hash_and_album` | exists check, 2 placeholders |
| `count_doubtful`, `list_doubtful` | heuristic WHERE clause |

### Group C — write_tx rewrites (~1h, 5 methods)

These currently use `self.db.connection().lock()` for atomicity. Port
to `write_tx`:

| Method | Why a tx | Notes |
|---|---|---|
| `create_batch` | N inserts share a prepared stmt | Use `write_tx`, inner loop calls `tx.execute(sql, params)` |
| `update_batch` | N updates share a prepared stmt | Same pattern |
| `deduplicate` | Multi-step read-then-write | Probably tx-friendly |
| `delete_all` | Currently 4 separate executes — make atomic | `write_tx` with the 4 DELETEs |
| `set_acoustid` (revised) | If we want it atomic with anything else | Optional |

### Group D — defer / SQLite-only (~30 min, ~5 methods)

These have constructs we don't have a clean PG story for yet:

| Method | Why deferred |
|---|---|
| `random_ids` | Uses `ORDER BY RANDOM()` — PG uses `ORDER BY random()` (lowercase) — easy add to dialect |
| `db()` getter | Returns `SqliteDb` — used by ~5 callers that build their own queries. Either remove (refactor callers) or keep with `self.sqlite_legacy.as_ref().expect(...)`. **Recommended**: keep with explicit panic on `with_backend` path until the callers are refactored. |
| `list_unidentified` | Uses `LIKE 'Track %'` and other string heuristics — already portable, but worth confirming with PG `LIKE` semantics (PG is case-sensitive, SQLite is not — wrap in `LOWER()`) |
| `deduplicate` | Complex multi-step logic — review carefully |

### Group E — row decoder (~30 min)

Add a `row_to_track(cols: &[SqlValue]) -> Track` mirroring the
existing `row_to_track(row: &rusqlite::Row)`. Then update every
`query_*` decode call.

Track has ~26 columns including `Option<i64>`, `Option<f64>`,
`Option<String>`, and a few special ones (`channels` defaults to 2,
`disc_number` defaults to 1, etc.). Mirror what's in the current
decoder.

Keep `row_to_track_rusqlite` for the legacy methods that still touch
rusqlite directly (Group D `db()` getter callers).

---

## Storage

```rust
pub struct TrackRepo {
    db: Arc<dyn DbBackend>,
    sqlite_legacy: Option<SqliteDb>,
}

impl TrackRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            sqlite_legacy: Some(db.clone()),
            db: Arc::new(db),
        }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db, sqlite_legacy: None }
    }

    /// Returns the SQLite handle. Panics on the `with_backend` path —
    /// the callers using this must be refactored before we can ship
    /// non-SQLite production. There are ~5 of them; the refactor is
    /// straightforward and lives in its own commit.
    pub fn db(&self) -> SqliteDb {
        self.sqlite_legacy
            .as_ref()
            .expect("track_repo.db() called on with_backend(); refactor caller")
            .clone()
    }
}
```

---

## Order of attack

Within the 4-6h session:

1. **0:00 — 0:30**: row_to_track for SqlValue + with_backend + db() panic
2. **0:30 — 1:30**: Group A — 20 mechanical ports
3. **1:30 — 2:30**: Group B — inline SQL → builders + port
4. **2:30 — 3:30**: Group C — write_tx rewrites
5. **3:30 — 4:30**: Group D — deferred + final review
6. **4:30 — 5:00**: Tests + cargo check -p tune-server + commit

Stop at any natural boundary — Group A alone is shippable progress.

---

## Caller audit for `db()` getter

Before / during the port, identify everywhere `track_repo.db()` is
called. From a quick grep, ~5 callers (mostly in the scanner). Each
either:
1. Builds its own SQLite-specific query (refactor to use a new
   `TrackRepo::*` method)
2. Passes the SqliteDb to another struct (carry the SqliteDb through
   `tune-core::AppState`, separate from the trait-object)

This audit is mandatory before the with_backend() path can be used in
production.

---

## What ships when

| Step | Sqlite behavior | Postgres-ready? |
|---|---|---|
| 1 (row_to_track + with_backend) | unchanged | partial — only the basic methods will work |
| 2 (Group A) | unchanged | yes (those 20 methods) |
| 3 (Group B) | unchanged | yes (those ~10 methods) |
| 4 (Group C) | unchanged | yes (5 batch/multi methods) |
| 5 (Group D) | unchanged | partial (db() callers stay SQLite for now) |
| 6 (tests + ship) | full coverage | 50/55 methods ready, db() audit follow-up |

After step 6: 13/13 repos ported. PG production rollout waits for the
`db()` caller refactor + history_repo Group C.

---

## Future: schema changes triggered by this port

Some columns referenced inline (e.g. `synced_lyrics`,
`acoustid_fingerprint`, `acoustid_confidence`, `trailing_silence_ms`,
`waveform_json`) need to land in `migrations/postgres/001_initial_schema.sql`
before the with_backend() path works for those methods. Add a single
migration `003_track_metadata_columns.sql` with the ALTERs.

---

## References

- Current file: `tune-core/src/db/track_repo.rs`
- Backend trait: `tune-core/src/db/backend.rs`
- Playbook: `docs/PORTING-REPOS-PLAYBOOK.md`
- Reference port for "monster with inline SQL": `album_repo` (commit `57b1e4c`)
- Reference port for "many builders + many decoders": `radio_repo`
- PG schema: `tune-core/migrations/postgres/001_initial_schema.sql`
