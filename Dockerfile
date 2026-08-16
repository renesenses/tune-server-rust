# ── Stage 0: Web client ──────────────────────────────────────────────
# Le client web n'est PAS dans ce dépôt (#1690) : il vit dans
# renesenses/tune-web-client et part en asset de release. On le construit ici,
# comme le fait .github/workflows/docker.yml pour l'image publiée — c'est la
# seule façon d'obtenir une image de dev dont l'interface correspond au code
# serveur qu'elle embarque. Branche `main` : c'est celle qui livre côté web
# (l'inverse du serveur, cf. CLAUDE.md).
FROM node:22-bookworm-slim AS web
ARG WEB_CLIENT_REF=main
RUN apt-get update && \
    apt-get install -y --no-install-recommends git ca-certificates && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /web
RUN git clone --depth 1 -b "$WEB_CLIENT_REF" \
      https://github.com/renesenses/tune-web-client.git . && \
    npm ci && npm run build

# ── Stage 1: Builder ─────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

# Install librespot + airplay-daemon build dependencies (cmake/clang are for
# airplay-daemon's vendored C encoders: fdk-aac + alac-encoder)
RUN apt-get update && \
    apt-get install -y --no-install-recommends libasound2-dev pkg-config cmake clang && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies: copy manifests and build with dummy sources
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p tune-core/src tune-server/src/routes tune-cli/src tune-ffi/src tune-bridge/src
COPY tune-core/Cargo.toml tune-core/
COPY tune-server/Cargo.toml tune-server/
COPY tune-cli/Cargo.toml tune-cli/
COPY tune-ffi/Cargo.toml tune-ffi/
COPY tune-bridge/Cargo.toml tune-bridge/
RUN echo 'fn main() {}' > tune-server/src/main.rs && \
    echo 'fn main() {}' > tune-cli/src/main.rs && \
    touch tune-core/src/lib.rs tune-server/src/lib.rs tune-ffi/src/lib.rs tune-bridge/src/lib.rs && \
    cargo build --release --package tune-server --no-default-features --features oaat,dj,karaoke,plugins-wasm,audio-embedding 2>/dev/null || true && \
    rm -rf tune-core/src tune-server/src tune-cli/src

# Build librespot (Spotify Connect) — optional, touch a placeholder if it fails
RUN cargo install librespot --no-default-features --features "alsa-backend" \
    || touch /usr/local/cargo/bin/librespot

# Build airplay-daemon (AirPlay 2 sender, #700) — GPL-2.0 standalone subprocess
# binary from a pinned rev of our fork. Optional in this dev image: on failure,
# touch a placeholder so a local build still succeeds (AirPlay 2 then falls back
# to legacy AirPlay 1). The release path (Dockerfile.dist) requires it.
RUN cargo install --git https://github.com/renesenses/airplay2-rs \
      --rev d87396a07ea8c3e16aa1d0525f5ef6d1a7626686 airplay-daemon --locked \
    || touch /usr/local/cargo/bin/airplay-daemon

# Build real source — clean dummy artifacts to force recompilation
COPY tune-core/ tune-core/
COPY tune-server/ tune-server/
COPY tune-cli/ tune-cli/
COPY tune-ffi/ tune-ffi/
COPY tune-bridge/ tune-bridge/
# Même jeu de features que .github/workflows/docker.yml : une image construite
# depuis ce Dockerfile sans audio-embedding renvoyait available:false et l'entrée
# Ambiance disparaissait de l'UI sans aucun message (#19 de la revue 2026-08-15).
RUN rm -rf target/release/.fingerprint/tune-* target/release/deps/tune_* target/release/deps/libtune_* target/release/tune-server && \
    cargo build --release --package tune-server --no-default-features --features oaat,dj,karaoke,plugins-wasm,audio-embedding && \
    strip target/release/tune-server

# ── Stage 2: Runtime ─────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates curl libasound2 python3-pip && \
    pip3 install --break-system-packages yt-dlp && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -g 1000 tune && \
    useradd -u 1000 -g tune -m -s /bin/false tune

WORKDIR /app

COPY --from=builder /build/target/release/tune-server /app/tune-server
COPY --from=builder /usr/local/cargo/bin/librespot /usr/local/bin/librespot
COPY --from=builder /usr/local/cargo/bin/airplay-daemon /usr/local/bin/airplay-daemon
COPY --from=web /web/dist/ /app/web/

# Ensure tune user can read the app but not write
RUN chown -R root:root /app && chmod -R 755 /app

# Create data + artwork_cache directories owned by tune
RUN mkdir -p /data/artwork_cache && chown -R tune:tune /data

ENV TUNE_PORT=8888 \
    TUNE_DB_PATH=/data/tune.db \
    TUNE_ARTWORK_DIR=/data/artwork_cache \
    TUNE_WEB_DIR=/app/web \
    TUNE_MUSIC_DIRS='["/music"]' \
    TUNE_LOG_LEVEL=info \
    TUNE_AUTO_SCAN=true \
    LIBRESPOT_NAME=Tune \
    LIBRESPOT_BITRATE=320

EXPOSE 8888

VOLUME ["/data", "/music"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -sf http://localhost:8888/api/v1/system/stats || exit 1

USER tune

ENTRYPOINT ["/app/tune-server"]
