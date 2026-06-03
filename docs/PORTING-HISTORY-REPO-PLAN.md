# Porting `history_repo` to `Arc<dyn DbBackend>` — detailed plan

`history_repo.rs` is the most complex of the remaining 7 repos:
- **952 LOC** (10 methods)
- **46 read sites**, **1 write site**
- 1 method (`full_dashboard`) is **300 LOC by itself**, builds 10
  sub-queries with conditional `WHERE` clauses, dynamic `format!`
  fragments, and joins to `albums` / `zones` / `tracks`
- Heavy use of SQLite-only date functions (`strftime`, `DATE()`)
  → now covered by the date helpers (`now_iso8601`, `since_days`,
  `date_trunc_day`) landed alongside `playlist_repo`
- 0 transactions → does not need `write_tx`

Estimate: **3-4 hours focused work** (a half-day session). Do not
bundle with other ports — the surface area is too large for the
"port + commit + push" cadence we've used for the previous repos.

---

## Pre-flight

Before starting, the following infra must be in place (all ✅ at the
time of writing):

- [x] `DbBackend::query_one` / `query_many` (landed in `fe66002`)
- [x] `SqlValue` decode helpers (landed in `fe66002`)
- [x] `SqlDialect::now_iso8601` / `since_days` / `date_trunc_day`
  (landed in this commit)
- [x] `format_fts_query` (already engine-aware)

The repo does **not** need `write_tx` (no atomic multi-write
operations).

---

## Method-by-method plan

### Group A — straight ports (easy, ~30 min)

| Method | LOC | Approach |
|---|---:|---|
| `record` | 15 | INSERT → use `DbBackend::execute`, `last_insert_rowid()` |
| `recent(limit)` | 12 | SELECT → `query_many` + `row_to_listen` decoder |
| `recent_paginated(limit, offset)` | 18 | SELECT + COUNT → 2 separate calls (no tx needed) |
| `top_tracks(limit)` | 18 | SELECT → `query_many` + tuple decode |
| `top_artists(limit)` | 17 | SELECT → `query_many` + tuple decode |
| `top_albums(limit)` | 18 | SELECT → `query_many` + tuple decode |
| `count()` | 6 | `query_one` + `as_i64()` |

All of these have **dialect-aware SQL builders already** (cf.
`history_repo::sql`). The port is identical pattern to
`source_link_repo` and `radio_repo`: swap `conn.query_row` /
`query_map` for `self.db.query_*`, decode via `SqlValue::as_*`.

### Group B — uses date helpers (~30 min)

| Method | LOC | Date function dependency |
|---|---:|---|
| `listening_history(days)` | 19 | `strftime('-N days')` → `dialect.since_days("listened_at", days)`. Also: `DATE(listened_at)` → `dialect.date_trunc_day("listened_at")`. |
| `dashboard()` | 28 | 4 sub-queries already in `sql::dashboard_*` builders. Straight port, no date helpers needed (counts/sums over all history). |

The SQL builders for `listening_history` need to be **promoted from
inline-string to dialect-driven**. Add a `sql::listening_history(d,
days)` builder:

```rust
pub fn listening_history<D: SqlDialect>(d: &D, days: i64) -> String {
    let day_col = d.date_trunc_day("listened_at");
    let since = d.since_days("listened_at", days);
    format!(
        "SELECT {day_col} as day, COUNT(*) as play_count, COALESCE(SUM(duration_ms), 0) as total_ms \
         FROM listen_history WHERE {since} \
         GROUP BY day ORDER BY day"
    )
}
```

Note: `days` becomes part of the SQL string (not a placeholder)
because both engines embed it in date arithmetic — there's no clean
way to parameterize the interval. The value is bounded (i64 from
trusted internal code) so injection is not a concern.

### Group C — the monster: `full_dashboard` (~2-3h)

The method builds:

| Sub-query | Lines | Complexity |
|---|---:|---|
| `from` (MIN listened_at) | ~15 | trivial, branch on period |
| `to` (now ISO) | ~5 | just emit `dialect.now_iso8601()` |
| `(plays, listening_ms, u_tracks, u_artists)` | ~12 | composite SELECT — keep as is |
| `top_artists` | ~24 | conditional WHERE, GROUP BY |
| `top_albums` | ~22 | LEFT JOIN albums, conditional WHERE |
| `top_tracks` | ~21 | conditional WHERE, GROUP BY |
| `trend` | ~28 | uses `DATE()`, `strftime('-N days')`, optional `zone_id` filter |
| `hourly` | ~18 | uses `strftime('%H', listened_at)` |
| `by_zone` | ~21 | LEFT JOIN zones, conditional WHERE |
| `by_source` | ~18 | GROUP BY source, conditional WHERE |
| `completion` | ~41 | nested subquery, `LEFT JOIN tracks`, conditional WHEREs |

**Strategy** — three sub-passes:

#### Pass 1: extract the conditional WHERE into helpers

The current code builds `simple_where` and `where_clause` (with `h.`
prefix) twice with conditionals on `days` and `zone_id`. Extract to:

```rust
struct DashFilters {
    days: Option<i64>,
    zone_id: Option<i64>,
}
impl DashFilters {
    fn simple_where(&self, d: &dyn SqlDialect) -> String { ... }
    fn aliased_where(&self, d: &dyn SqlDialect, alias: &str) -> String { ... }
}
```

Replace inline `strftime` with `d.since_days("listened_at", n)` and
`format!("zone_id = {zid}")` with a parameterized placeholder if
zone_id is known up front (we can avoid SQL injection by using
parameter binding — the current code embeds the int directly, which
is safe but not idiomatic).

#### Pass 2: promote each sub-query to a SQL builder

Add 10 new functions to `history_repo::sql`:

- `sql::dashboard_range_from(d, filters) -> String`
- `sql::dashboard_totals(d, filters) -> String`
- `sql::dashboard_top_artists(d, filters) -> String`
- `sql::dashboard_top_albums(d, filters) -> String`
- `sql::dashboard_top_tracks(d, filters) -> String`
- `sql::dashboard_trend(d, days, zone_id) -> String`
- `sql::dashboard_hourly(d, filters) -> String`
- `sql::dashboard_by_zone(d, filters) -> String`
- `sql::dashboard_by_source(d, filters) -> String`
- `sql::dashboard_completion(d, filters) -> String`

Each takes the dialect + filter struct + the `top_n` placeholder
where needed. They emit complete SQL ready to be passed to
`DbBackend::query_*`.

Add unit tests asserting both SQLite and Postgres dialects produce
plausible SQL (no `strftime` in PG output, no `to_char` in SQLite,
correct placeholder syntax). Mirror the pattern used in
`source_link_repo::sql_builders_dialect_placeholders`.

#### Pass 3: rewrite `full_dashboard` against the builders

Replace the 10 `conn.prepare(format!(...))` blocks with:

```rust
let rows = self.db.query_many(&sql::dashboard_top_artists(&d, &filters), &[&top_n])?;
let top_artists: Vec<TopArtistEntry> = rows.iter().map(row_to_top_artist).collect();
```

One `row_to_*` decoder per result struct. The `period` / `from` /
`to` logic stays in the Rust code (date math, period parsing).

#### Special cases

- **`hourly`** uses `strftime('%H', listened_at)` which is SQLite-only.
  Add a dialect helper `extract_hour(column)`:
  - SQLite: `CAST(strftime('%H', column) AS INTEGER)`
  - Postgres: `EXTRACT(HOUR FROM column::timestamp)::int`

- **Embedded `zone_id` in SQL string**. The current code does
  `format!("zone_id = {zid}")`. Two options:
  1. Keep that (zone_id is internal i64, no injection risk)
  2. Parameterize via `query_many` params
  Option 1 is consistent with how `top_n` is currently embedded;
  option 2 is cleaner. Prefer option 2 — it's the same effort and
  removes any future risk if `zone_id` source ever changes.

- **`completion`'s nested subquery** is the worst single piece — a
  subquery joining `listen_history` and `tracks` with both having
  filters. Best handled as a single named SQL builder
  (`sql::dashboard_completion`) that takes the filters once and
  produces the whole 30-line statement.

---

## Test coverage

Existing tests (tail of the file): some integration tests around
`record` / `recent`. Add:

- **Per-builder dialect tests** — assert no `strftime` in PG, no
  `to_char` in SQLite, placeholders syntax correct.
- **`full_dashboard` smoke test** on SQLite with seeded data
  covering each period (`"7d"`, `"30d"`, `"all"`) — assert the
  returned structure has the expected counts.
- **`with_backend_constructor` test** as in the previous 7 ports.

---

## Cutover

1. Land a separate PR with **just** the new `sql::dashboard_*` builders
   + their tests. Zero behavior change yet.
2. Land a second PR rewriting `full_dashboard` to use the builders.
   This is the risk-bearing change — review carefully.
3. Land a third PR doing the simple ports (Groups A + B) — minimal
   risk.

Three small PRs > one giant one. Same total work, much cleaner
review surface, easier to bisect if a regression sneaks in.

---

## Order of attack

Within the half-day session:

1. **0:00 — 0:45**: Group A ports (7 methods, mechanical)
2. **0:45 — 1:15**: Group B ports (2 methods, date helpers)
3. **1:15 — 1:30**: `extract_hour` dialect helper + test
4. **1:30 — 2:30**: `full_dashboard` Pass 1+2 — SQL builders only
5. **2:30 — 3:30**: `full_dashboard` Pass 3 — rewrite method body
6. **3:30 — 4:00**: Tests + verify tune-server compiles + commit

Stop at any natural boundary if tired. The file compiles after each
sub-group.

---

## What ships when

| Step | Sqlite behavior | Postgres-ready? |
|---|---|---|
| 1 (Group A) | Same as before | Yes (all builders dialect-aware) |
| 2 (Group B) | Same as before | Yes |
| 3 (extract_hour helper) | N/A | Yes |
| 4 (dashboard builders) | Old code still in use, no change | Yes |
| 5 (dashboard rewrite) | Same dashboard, new code path | Yes |
| 6 (tests + ship) | Confidence | Yes |

After step 6: 8/13 repos ported, only zone/artist/play_queue/album/track
remain (all 5 use transactions, all 5 need `write_tx`).

---

## Future: schema changes triggered by this port

None. The schema is unchanged — only SQL emission changes.

---

## References

- Current file: `tune-core/src/db/history_repo.rs`
- Date helpers: `tune-core/src/db/engine.rs` (`SqlDialect::since_days`,
  `now_iso8601`, `date_trunc_day`)
- Backend trait: `tune-core/src/db/backend.rs` (`query_one`,
  `query_many`)
- Playbook: `docs/PORTING-REPOS-PLAYBOOK.md` (general pattern)
- Reference port for "many builders + many decoders": `radio_repo`
