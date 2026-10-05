#!/usr/bin/env bash
set -euo pipefail

# Tune Server — Docker Quick Install
# Usage:
#   curl -sSL https://raw.githubusercontent.com/renesenses/tune-server-rust/main/scripts/docker-install.sh \
#     | bash -s -- --music /path/of/your/music
#   (or TUNE_MUSIC_PATH=/path/of/your/music; without either, the script asks
#   on the terminal, and refuses to guess when there is none.)

TUNE_IMAGE="renesenses/tune:latest"
INSTALL_DIR="${TUNE_INSTALL_DIR:-$HOME/tune-server}"
MUSIC_PATH="${TUNE_MUSIC_PATH:-}"

usage() {
    echo "Usage: docker-install.sh --music <music folder on this machine>"
    echo "       (or set TUNE_MUSIC_PATH; TUNE_INSTALL_DIR overrides $HOME/tune-server)"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --music)
            [ $# -ge 2 ] || { usage >&2; exit 2; }
            MUSIC_PATH="$2"
            shift 2
            ;;
        --music=*)
            MUSIC_PATH="${1#--music=}"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

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
ORIG_PWD="$PWD"
mkdir -p "$INSTALL_DIR"
cd "$INSTALL_DIR"

# Music folder: argument, environment, the previous install (.env), or a
# question on the terminal. Never a placeholder: docker would create an empty
# /path/to/music as root and Tune would scan nothing.
if [ -z "$MUSIC_PATH" ] && [ -f .env ]; then
    MUSIC_PATH="$(sed -n "s/^TUNE_MUSIC_PATH='\(.*\)'$/\1/p" .env | tail -n1)"
fi
# `curl | bash`: stdin is the script itself, so ask on the terminal — when
# there is one (the subshell probes it without risking `set -e`).
if [ -z "$MUSIC_PATH" ] && (: </dev/tty) 2>/dev/null; then
    printf 'Music folder on this machine (mounted read-only on /music): ' >/dev/tty
    IFS= read -r MUSIC_PATH </dev/tty || true
fi
if [ -z "$MUSIC_PATH" ]; then
    echo "ERROR: no music folder given." >&2
    usage >&2
    exit 2
fi
case "$MUSIC_PATH" in
    /*) ;;
    *) MUSIC_PATH="$(cd "$ORIG_PWD" && cd "$MUSIC_PATH" 2>/dev/null && pwd)" \
        || { echo "ERROR: music folder not found: $MUSIC_PATH" >&2; exit 2; } ;;
esac
if [ ! -d "$MUSIC_PATH" ]; then
    echo "ERROR: music folder not found: $MUSIC_PATH" >&2
    exit 2
fi
case "$MUSIC_PATH" in
    *"'"*) echo "ERROR: the music folder path cannot contain a single quote: $MUSIC_PATH" >&2; exit 2 ;;
esac
echo "Music folder: $MUSIC_PATH"

# .env is read by `docker compose` itself, to fill in ${TUNE_MUSIC_PATH}
# below. (.env.tune is passed to the container: it cannot do that.)
printf "TUNE_MUSIC_PATH='%s'\n" "$MUSIC_PATH" > .env

# Create .env.tune if it doesn't exist
if [ ! -f .env.tune ]; then
    cat > .env.tune <<'ENVEOF'
## Edit this file to configure Tune Server (then: docker compose up -d)
TUNE_PORT=8888
# Same value as the image's default: the database lives in the tune-data volume.
TUNE_DB_PATH=/data/tune.db
TUNE_LOG_LEVEL=info
TUNE_AUTO_SCAN=true
TUNE_MUSIC_DIRS=["/music"]
ENVEOF
    echo "Created $INSTALL_DIR/.env.tune"
fi

# Create docker-compose.yml
# No memory limit: none was ever measured, and a scan or an analysis of a large
# library goes past 512M — the kernel then kills Tune mid-scan (OOM), which
# looks like a crash. Add `deploy.resources.limits.memory` yourself if needed.
cat > docker-compose.yml <<'COMPOSEEOF'
services:
  tune:
    image: renesenses/tune:latest
    container_name: tune-server
    restart: unless-stopped
    network_mode: host
    volumes:
      - tune-data:/data
      - ${TUNE_MUSIC_PATH:?set TUNE_MUSIC_PATH in .env}:/music:ro
    env_file:
      - .env.tune

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
echo "  1. cd $INSTALL_DIR"
echo "  2. docker compose up -d"
echo "  3. Open http://localhost:8888"
echo "  (music folder: $INSTALL_DIR/.env, settings: $INSTALL_DIR/.env.tune)"
echo ""
echo "Useful commands:"
echo "  docker compose logs -f          # View logs"
echo "  docker compose restart          # Restart"
echo "  docker compose pull && docker compose up -d  # Update"
