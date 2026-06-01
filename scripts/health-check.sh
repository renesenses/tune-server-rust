#!/bin/bash
# Health check for known Tune server instances.
# Run via cron: */30 * * * * /path/to/health-check.sh
#
# Checks each server's /api/v1/system/version endpoint and alerts
# if the service is down or running an outdated version.

EXPECTED_VERSION="0.8.15"
TIMEOUT=5

SERVERS=(
    "192.168.1.18:8888|.18 (staging)"
    "192.168.1.15:8888|.15 (prod)"
)

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

errors=0

for entry in "${SERVERS[@]}"; do
    IFS='|' read -r addr name <<< "$entry"
    resp=$(curl -s --max-time "$TIMEOUT" "http://$addr/api/v1/system/version" 2>/dev/null)

    if [ -z "$resp" ]; then
        echo -e "${RED}DOWN${NC} $name ($addr) — no response"
        errors=$((errors + 1))
        continue
    fi

    version=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('version','?'))" 2>/dev/null)
    engine=$(echo "$resp" | python3 -c "import json,sys; print(json.load(sys.stdin).get('engine','?'))" 2>/dev/null)

    if [ "$version" = "$EXPECTED_VERSION" ]; then
        echo -e "${GREEN}  OK${NC} $name — v$version ($engine)"
    else
        echo -e "${YELLOW}WARN${NC} $name — v$version (expected $EXPECTED_VERSION)"
    fi
done

if [ $errors -gt 0 ]; then
    exit 1
fi
