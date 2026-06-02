#!/bin/bash
# Snapshot the API performance from a running Tune server and generate a markdown report.
#
# Usage:
#   ./scripts/perf-baseline.sh [server_url]
#
# Default server: http://192.168.1.15:8888
# Output: docs/perf-baseline-YYYY-MM-DD.md

set -euo pipefail

SERVER="${1:-http://192.168.1.15:8888}"
DATE=$(date +%Y-%m-%d)
TIME=$(date +%H:%M)
OUTPUT="docs/perf-baseline-${DATE}.md"

echo "Fetching api-stats from ${SERVER} ..."
STATS=$(curl -s --max-time 10 "${SERVER}/api/v1/system/api-stats")

if [ -z "$STATS" ] || ! echo "$STATS" | python3 -c "import json,sys; json.load(sys.stdin)" 2>/dev/null; then
    echo "ERROR: could not fetch valid stats from ${SERVER}"
    exit 1
fi

VERSION=$(curl -s --max-time 5 "${SERVER}/api/v1/system/version" | python3 -c "import json,sys; print(json.load(sys.stdin).get('version','?'))")
DIAG=$(curl -s --max-time 5 "${SERVER}/api/v1/system/diagnostics")
TRACKS=$(echo "$DIAG" | python3 -c "import json,sys; print(json.load(sys.stdin).get('tracks_count', 0))")
ALBUMS=$(echo "$DIAG" | python3 -c "import json,sys; print(json.load(sys.stdin).get('albums_count', 0))")
UPTIME=$(echo "$DIAG" | python3 -c "import json,sys; print(json.load(sys.stdin).get('uptime_seconds', 0))")

mkdir -p docs

python3 << PYEOF > "${OUTPUT}"
import json
stats = json.loads('''${STATS}''')

print(f"# Baseline performance — Tune v${VERSION}")
print()
print(f"**Server** : ${SERVER}")
print(f"**Date** : ${DATE} ${TIME}")
print(f"**Library** : ${TRACKS} pistes, ${ALBUMS} albums")
print(f"**Server uptime** : ${UPTIME} secondes")
print()
print(f"**Total requests analyzed** : {stats['total_requests']}")
print(f"**Total errors** : {stats['total_errors']}")
print(f"**Error rate** : {stats['error_rate_pct']}%")
print()
print("## Top 10 endpoints les plus appelés")
print()
print("| # | Endpoint | Count | Avg ms | P50 | P95 | P99 | Max |")
print("|---|----------|-------|--------|-----|-----|-----|-----|")
for i, ep in enumerate(stats['top_endpoints'][:10], 1):
    print(f"| {i} | \`{ep['endpoint']}\` | {ep['count']} | {ep['avg_latency_ms']} | {ep['p50_latency_ms']} | {ep['p95_latency_ms']} | {ep['p99_latency_ms']} | {ep['max_latency_ms']} |")
print()
print("## Top 10 endpoints les plus lents (P95)")
print()
print("| # | Endpoint | Count | Avg ms | P50 | P95 | P99 | Max | Errors |")
print("|---|----------|-------|--------|-----|-----|-----|-----|--------|")
for i, ep in enumerate(stats['slowest_endpoints'][:10], 1):
    print(f"| {i} | \`{ep['endpoint']}\` | {ep['count']} | {ep['avg_latency_ms']} | {ep['p50_latency_ms']} | {ep['p95_latency_ms']} | {ep['p99_latency_ms']} | {ep['max_latency_ms']} | {ep['error_count']} |")
print()
print("## Conclusion")
print()
print("Ce rapport sert de baseline pour comparer la performance future après les optimisations.")
print()
print("Seuils indicatifs :")
print()
print("- **P95 < 100ms** : excellent")
print("- **P95 < 500ms** : acceptable")
print("- **P95 > 500ms** : à optimiser")
print()
PYEOF

echo "Report generated: ${OUTPUT}"
