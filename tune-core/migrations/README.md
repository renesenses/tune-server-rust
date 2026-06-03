# Tune migrations

Two parallel tracks for the same schema, picked at boot based on the
configured `database.engine`.

```
tune-core/migrations/
├── sqlite/        # SQLite-specific SQL (FTS5 virtual tables, PRAGMAs)
└── postgres/      # PostgreSQL-specific SQL (BIGSERIAL, tsvector, GIN)
```

Each file is named `NNN_short_description.sql`. The number is the
migration version; the loader applies them in numeric order and
records the applied version in `schema_version`.

## Current state

- `postgres/001_initial_schema.sql` — bootstrap script for a fresh
  Postgres database, hand-translated from the SQLite CORE_SCHEMA in
  `tune-core/src/db/sqlite.rs`.
- `sqlite/` — empty for now. The existing SQLite migrations stay
  inline in `tune-core/src/db/migrations.rs` until phase 3 of the PG
  roadmap moves them here.

## Bootstrap a fresh PostgreSQL database

```bash
psql -h localhost -U tune -d tune -f tune-core/migrations/postgres/001_initial_schema.sql
```

Then point Tune at it:

```toml
# tune.toml
[database]
engine = "postgres"
connection_string = "postgresql://tune:tune@localhost:5432/tune"
```

Or via env var:

```bash
TUNE_DATABASE_URL=postgresql://tune:tune@localhost:5432/tune tune-server
```

## Translation conventions (SQLite → Postgres)

| SQLite | Postgres |
|--------|----------|
| `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL PRIMARY KEY` |
| `INTEGER REFERENCES x(id)`          | `BIGINT REFERENCES x(id)` |
| `REAL`                              | `DOUBLE PRECISION` |
| `INTEGER DEFAULT 0/1` (boolean)     | `SMALLINT DEFAULT 0/1` (keep 0/1 semantics for back-compat; promotion to `BOOLEAN` is a follow-up migration) |
| `INTEGER` for large counts (duration_ms, file_size) | `BIGINT` |
| `TEXT`                              | `TEXT` |
| `FTS5 virtual table`                | `tsvector` column + `GIN` index (phase 4 of PG roadmap, file `002_fts_tsvector.sql`, not yet written) |
| `strftime('%Y-%m-%dT%H:%M:%SZ', 'now')` | `to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')` (handled by dialect helper, not the schema) |
| `json_each(col)`                    | `jsonb_array_elements_text(col)` (handled by dialect helper, not the schema) |

## What's NOT in `001_initial_schema.sql` yet

- FTS5 → tsvector migration (will be `002_fts_tsvector.sql`)
- All the incremental migrations 2..N that exist in the SQLite
  monolith (will be hand-translated when phase 3 moves them here)
- Application-level migrations runner integration

See `docs/POSTGRES-PLAN.md` for the full PG roadmap and the phase
ordering.
