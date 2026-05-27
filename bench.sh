#!/bin/bash
# Tune Server Rust — Performance Benchmarks
# Usage: ./bench.sh [host:port]

HOST=${1:-localhost:8085}
echo "=== Tune Rust Server Benchmarks ==="
echo "Target: $HOST"
echo "Date:   $(date -Iseconds)"
echo

# Check server is reachable
if ! curl -sf "http://$HOST/api/v1/system/health" > /dev/null 2>&1; then
    echo "ERROR: Server not reachable at http://$HOST"
    exit 1
fi

# API latency
echo "--- API Latency ---"
for endpoint in \
    "/api/v1/system/health" \
    "/api/v1/system/version" \
    "/api/v1/system/diagnostics" \
    "/api/v1/library/tracks?limit=50" \
    "/api/v1/library/albums?limit=50" \
    "/api/v1/library/artists?limit=50" \
    "/api/v1/library/search?q=miles&limit=20" \
    "/api/v1/library/search?q=love&limit=20" \
    "/api/v1/library/stats" \
    "/api/v1/streaming/services" \
    "/api/v1/zones" \
    "/api/v1/playlists"; do
    time_s=$(curl -s -o /dev/null -w '%{time_total}' "http://$HOST$endpoint")
    time_ms=$(echo "$time_s" | tr ',' '.' | awk '{printf "%.1f", $1 * 1000}')
    printf "  %-55s %sms\n" "$endpoint" "$time_ms"
done

# Repeated measurement for key endpoints (5 runs, report avg)
echo
echo "--- Averaged Latency (5 runs) ---"
for endpoint in "/api/v1/system/health" "/api/v1/library/tracks?limit=50" "/api/v1/library/search?q=miles&limit=20"; do
    total=0
    for _ in 1 2 3 4 5; do
        t=$(curl -s -o /dev/null -w '%{time_total}' "http://$HOST$endpoint")
        t_clean=$(echo "$t" | tr ',' '.')
        total=$(echo "$total + $t_clean" | bc)
    done
    avg=$(echo "$total / 5 * 1000" | bc -l)
    printf "  %-55s %.1fms (avg 5 runs)\n" "$endpoint" "$avg"
done

# Version + diagnostics
echo
echo "--- Server Info ---"
curl -s "http://$HOST/api/v1/system/diagnostics" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(f\"  Version:  {d.get('version')} ({d.get('engine')})\")
    print(f\"  Platform: {d.get('platform')} {d.get('arch')}\")
    print(f\"  CPUs:     {d.get('cpu_count')}\")
    print(f\"  Uptime:   {d.get('uptime_seconds')}s\")
    print(f\"  PID:      {d.get('pid')}\")
    print(f\"  FFmpeg:   {d.get('ffmpeg_available')}\")
    db = d.get('db', {})
    print(f\"  DB:       {db.get('engine')} (migration v{db.get('migration_version')})\")
    re = d.get('rust_engines', {})
    if re:
        print(f\"  Engines:  metadata={re.get('metadata_engine')}, scanner={re.get('scanner_engine')}, discovery={re.get('discovery_engine')}\")
except Exception as e:
    print(f'  (parse error: {e})')
" 2>/dev/null

# Library stats
echo
echo "--- Library ---"
curl -s "http://$HOST/api/v1/library/stats" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print(f\"  Tracks:   {d.get('tracks', 'N/A'):,}\")
    print(f\"  Albums:   {d.get('albums', 'N/A'):,}\")
    print(f\"  Artists:  {d.get('artists', 'N/A'):,}\")
    print(f\"  Listens:  {d.get('listens', 'N/A'):,}\")
    ms = d.get('total_duration_ms', 0)
    if ms:
        hours = ms / 3_600_000
        print(f\"  Duration: {hours:,.0f} hours\")
    size = d.get('total_size_bytes', 0)
    if size:
        gb = size / (1024**3)
        print(f\"  Size:     {gb:,.1f} GB\")
except Exception as e:
    print(f'  (parse error: {e})')
" 2>/dev/null

# Memory (Linux only)
echo
echo "--- Memory (RSS) ---"
if command -v pgrep &> /dev/null; then
    pid=$(curl -s "http://$HOST/api/v1/system/diagnostics" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pid',''))" 2>/dev/null)
    if [ -n "$pid" ] && [ -f "/proc/$pid/status" ]; then
        rss=$(grep VmRSS /proc/$pid/status 2>/dev/null | awk '{print $2}')
        if [ -n "$rss" ]; then
            mb=$((rss / 1024))
            printf "  RSS: %s MB (PID %s)\n" "$mb" "$pid"
        else
            echo "  (process not local or /proc not available)"
        fi
    else
        echo "  (server not running locally or PID $pid not accessible)"
    fi
else
    echo "  (pgrep not available)"
fi

echo
echo "=== Done ==="
