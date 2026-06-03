# Deploying Tune on PostgreSQL

How to run tune-server with PostgreSQL as the backing store instead
of SQLite. Reference: [POSTGRES-PLAN.md](POSTGRES-PLAN.md).

## Current support

- ✅ Connection at boot (PgPool dans `AppState`, optional)
- ✅ Bootstrap schema (`tune-core/migrations/postgres/001_initial_schema.sql`)
- ✅ Full-text search (`tune-core/migrations/postgres/002_fts_tsvector.sql`)
- ✅ One-way migration tool (`tune db migrate-to-postgres`)
- ✅ Engine-agnostic SQL via `SqlDialect` (13 repos ported)
- ⚠️  **Runtime repos still write to SQLite** even when the
   PostgresDb pool is open. Phase 5 (the big refactor — repos taking
   `Arc<dyn DbBackend>`) is what flips the actual data path. Until
   then PG is a smoke-test connection only.

In practice: **SQLite is still the only engine actually serving
read/write traffic at runtime, but the entire data layer can be
exercised against PG via `tune db migrate-to-postgres` + ad-hoc
psql.**

## Prerequisites

- PostgreSQL 16 (15 also works for 001+002; 14 has not been tested)
- `unaccent` extension available (bundled with PG 16, contrib package
  on PG 15)
- A tune-server binary built with the `postgres` feature:

```bash
cargo build --release -p tune-server --features postgres
```

The default build does NOT include the postgres feature; the
PostgresDb code is compiled out. The binary can still read a SQLite
DB normally.

For tune-cli:

```bash
cargo install --path tune-cli --features postgres
```

## Step 1 — Bootstrap a fresh PostgreSQL database

```bash
# Assuming a vanilla PG instance running locally on port 5432
createdb tune

psql -d tune -f tune-core/migrations/postgres/001_initial_schema.sql
psql -d tune -f tune-core/migrations/postgres/002_fts_tsvector.sql
```

Expected output: 9 tables, 8 indexes, 3 tsvector columns + GIN
indexes, schema_version v1 and v2 recorded.

Verify:

```bash
psql -d tune -c "\dt"
psql -d tune -c "SELECT version, name FROM schema_version"
```

## Step 2 — Migrate existing SQLite data (one-shot)

If you have a SQLite tune.db with content you want to preserve:

```bash
tune db migrate-to-postgres \
    --from tune.db \
    --to postgresql://user:pass@localhost:5432/tune \
    --batch-size 500
```

The tool walks 8 core tables in topological order
(artists → albums → tracks → track_credits → playlists →
playlist_tracks → zones → play_queue), batches rows in a single
transaction per batch, uses `ON CONFLICT DO NOTHING` for
idempotence, and verifies row counts at the end.

For a large library (100k+ tracks), expect ~5-15 minutes depending
on disk and network. Increase `--batch-size` (up to 10000) for a
modest speedup.

`schema_version` is intentionally not migrated — the PG bootstrap
manages its own copy.

Re-running the migration is safe: `ON CONFLICT DO NOTHING` on the
primary key skips already-imported rows.

## Step 3 — Point tune-server at PostgreSQL

Either via `tune.toml`:

```toml
[database]
engine = "postgres"
connection_string = "postgresql://user:pass@localhost:5432/tune"
max_connections = 16
acquire_timeout_secs = 5
```

Or via env var:

```bash
TUNE_DATABASE_URL=postgresql://user:pass@localhost:5432/tune ./tune-server
```

Boot logs should show:

```
INFO postgres_connected server_version="PostgreSQL 16.x ..."
```

If you see `postgres_engine_selected_without_connection_string`, the
`connection_string` is missing from config and the env var didn't
match either.

If you see `postgres_open_failed error="…"`, the DSN is wrong or
the server is unreachable. The server keeps booting on SQLite in
that case (postgres is opportunistic at this stage).

## Step 4 — Verify

`GET /api/v1/system/database/status` returns the current engine
information:

```json
{
  "engine": "sqlite",            // still sqlite, until phase 5 flips it
  "migration_version": …,
  ...
}
```

`POST /api/v1/system/database/test-connection` now actually opens
a fresh PG pool (not a stub):

```bash
curl -X POST http://localhost:8888/api/v1/system/database/test-connection \
    -H 'Content-Type: application/json' \
    -d '{"engine":"postgresql","connection_string":"postgresql://user:pass@localhost:5432/tune"}'
```

Expected:

```json
{
  "status": "ok",
  "engine": "postgresql",
  "server_version": "PostgreSQL 16.x ..."
}
```

## Step 5 — Run a search via psql

Demonstrates the FTS layer (phase 4 of the PG plan):

```sql
-- Accent-insensitive search ("stromae" finds "Stromaé"):
SELECT id, name FROM artists
WHERE search_tsv @@ to_tsquery('simple', unaccent('stromae:*'));

-- Track search with denormalized artist name:
SELECT t.id, t.title, ar.name
FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id
WHERE t.search_tsv @@ to_tsquery('simple', unaccent('miles:*'));
```

The repos' `search()` methods now emit exactly these queries when
the engine is Postgres — they pick the right shape via
`dialect.fts_where` and `to_tsquery('simple', unaccent(?))`
(see PR with commit `9b0d473`).

## What does NOT work yet

These are tracked in phase 5 of POSTGRES-PLAN.md:

- The runtime data path still goes through `SqliteDb` — the
  PostgresDb pool is only used by `/system/database/test-connection`
  and the boot smoke-check. **Phase 5 flips this** by introducing
  the trait `DbBackend` and making the 13 repos take `Arc<dyn
  DbBackend>` instead of `SqliteDb`.
- `strftime(...)` / `DATE()` calls in repos (history dashboard,
  recordings) are not portable yet. They will use
  `dialect.date_now_iso()` style helpers in phase 5.
- `json_each(...)` in `album_repo.list_by_genre` is SQLite-only;
  PG equivalent (`jsonb_array_elements_text`) waits for phase 5.
- The ~50 incremental SQLite migrations (settings, recordings,
  ratings, etc.) have not been ported to PG-flavored migrations
  yet. Only the core 001 schema + the FTS 002 are PG-ready.
- The `tune db migrate-to-postgres` tool migrates the
  001-bootstrap shape only. Tables added by migrations 2+ will
  be missing from the PG-side until 002+.sql files land.

## Rollback (PG → SQLite)

There is no automatic rollback. The intent is one-way migration.
If you need to go back:

1. Stop tune-server
2. Set `engine = "sqlite"` (or unset `TUNE_DATABASE_URL`)
3. Restart — SQLite is the default

The original SQLite file is never touched by the migration tool, so
it remains intact and you can fall back to it at any time. Just
remember the PG-only data (anything created between the migration
and the rollback) will not flow back.

---

*Document évolutif. Dernière mise à jour : 2026-06-03.*
