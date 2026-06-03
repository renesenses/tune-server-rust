#!/usr/bin/env bash
# pg-e2e.sh — spin up a disposable PostgreSQL in Docker, apply the
# Tune migrations, then run cargo tests gated on the
# TUNE_TEST_PG_URL env var. Cleans up the container on exit.
#
# Usage:
#   ./scripts/pg-e2e.sh                    # full cycle
#   ./scripts/pg-e2e.sh --keep             # leave the container running
#   PG_PORT=55432 ./scripts/pg-e2e.sh      # override port
#
# Requirements: docker (rootless or sudo-less daemon).

set -euo pipefail

PG_PORT="${PG_PORT:-55432}"
PG_USER="tune"
PG_PASS="tune"
PG_DB="tune_test"
CONTAINER="tune-pg-e2e"
KEEP_RUNNING="${1:-}"

cleanup() {
    if [[ "$KEEP_RUNNING" != "--keep" ]]; then
        echo "─── Cleaning up container ${CONTAINER} ─────────────────"
        docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
    else
        echo "─── Leaving container ${CONTAINER} running on :${PG_PORT} ─"
    fi
}
trap cleanup EXIT

echo "─── Starting Postgres 16 on :${PG_PORT} ────────────────"
docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
docker run -d --name "${CONTAINER}" \
    -e POSTGRES_USER="${PG_USER}" \
    -e POSTGRES_PASSWORD="${PG_PASS}" \
    -e POSTGRES_DB="${PG_DB}" \
    -p "${PG_PORT}":5432 \
    postgres:16-alpine >/dev/null

echo "─── Waiting for Postgres to accept connections ─────────"
for i in $(seq 1 30); do
    if docker exec "${CONTAINER}" pg_isready -U "${PG_USER}" -d "${PG_DB}" >/dev/null 2>&1; then
        echo "  ready after ${i}s"
        break
    fi
    sleep 1
done

# Postgres needs the unaccent extension for 002_fts_tsvector.sql.
echo "─── Installing extensions (unaccent) ───────────────────"
docker exec "${CONTAINER}" psql -U "${PG_USER}" -d "${PG_DB}" \
    -c "CREATE EXTENSION IF NOT EXISTS unaccent;" >/dev/null

echo "─── Applying migrations ────────────────────────────────"
for migration in tune-core/migrations/postgres/*.sql; do
    name=$(basename "${migration}")
    echo "  ${name}"
    docker exec -i "${CONTAINER}" psql -U "${PG_USER}" -d "${PG_DB}" \
        -v ON_ERROR_STOP=1 -q < "${migration}"
done

PG_URL="postgresql://${PG_USER}:${PG_PASS}@localhost:${PG_PORT}/${PG_DB}"

echo
echo "─── Running cargo test --features postgres ─────────────"
echo "    TUNE_TEST_PG_URL=${PG_URL}"
echo

export TUNE_TEST_PG_URL="${PG_URL}"
cargo test -p tune-core --features postgres --lib -- postgres_e2e --nocapture
