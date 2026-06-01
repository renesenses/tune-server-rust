#!/usr/bin/env bash
set -euo pipefail

# Tune Server — Docker Quick Install
# Usage: curl -sSL https://raw.githubusercontent.com/renesenses/tune-server-rust/main/scripts/docker-install.sh | bash

TUNE_IMAGE="renesenses/tune:latest"
INSTALL_DIR="${TUNE_INSTALL_DIR:-$HOME/tune-server}"

echo "=== Tune Server Docker Install ==="
echo ""

# Check Docker
if ! command -v docker &>/dev/null; then
    echo "ERROR: Docker is not installed. Install Docker first:"
    echo "  https://docs.docker.com/get-docker/"
    exit 1
fi

if ! docker info &>/dev/null; then
    echo "ERROR: Docker daemon is not running."
    exit 1
fi

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64) PLATFORM="linux/amd64" ;;
    aarch64|arm64) PLATFORM="linux/arm64" ;;
    *)
        echo "WARNING: Unsupported architecture $ARCH, attempting anyway."
        PLATFORM=""
        ;;
esac
echo "Architecture: $ARCH ($PLATFORM)"

# Create install directory
mkdir -p "$INSTALL_DIR"
cd "$INSTALL_DIR"

# Create .env.tune if it doesn't exist
if [ ! -f .env.tune ]; then
    cat > .env.tune <<'ENVEOF'
## Edit this file to configure Tune Server
TUNE_MUSIC_PATH=/path/to/your/music
TUNE_PORT=8888
TUNE_DB_PATH=/data/tune_v2.db
TUNE_LOG_LEVEL=info
TUNE_AUTO_SCAN=true
TUNE_MUSIC_DIRS=["/music"]
ENVEOF
    echo "Created $INSTALL_DIR/.env.tune — edit TUNE_MUSIC_PATH before starting."
fi

# Create docker-compose.yml
cat > docker-compose.yml <<'COMPOSEEOF'
services:
  tune:
    image: renesenses/tune:latest
    container_name: tune-server
    restart: unless-stopped
    network_mode: host
    volumes:
      - tune-data:/data
      - ${TUNE_MUSIC_PATH:-/path/to/music}:/music:ro
    env_file:
      - .env.tune
    deploy:
      resources:
        limits:
          memory: 512M

volumes:
  tune-data:
COMPOSEEOF

# Pull image
echo ""
echo "Pulling $TUNE_IMAGE..."
if [ -n "$PLATFORM" ]; then
    docker pull --platform "$PLATFORM" "$TUNE_IMAGE"
else
    docker pull "$TUNE_IMAGE"
fi

echo ""
echo "=== Installation complete ==="
echo ""
echo "Next steps:"
echo "  1. Edit $INSTALL_DIR/.env.tune (set TUNE_MUSIC_PATH)"
echo "  2. cd $INSTALL_DIR"
echo "  3. docker compose up -d"
echo "  4. Open http://localhost:8888"
echo ""
echo "Useful commands:"
echo "  docker compose logs -f          # View logs"
echo "  docker compose restart          # Restart"
echo "  docker compose pull && docker compose up -d  # Update"
