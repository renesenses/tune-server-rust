# ── Stage 1: Builder ─────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

# Install librespot build dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends libasound2-dev pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies: copy manifests and build with dummy sources
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p tune-core/src tune-pyo3/src tune-server/src/routes tune-cli/src tune-ffi/src tune-bridge/src
COPY tune-core/Cargo.toml tune-core/
COPY tune-pyo3/Cargo.toml tune-pyo3/
COPY tune-server/Cargo.toml tune-server/
COPY tune-cli/Cargo.toml tune-cli/
COPY tune-ffi/Cargo.toml tune-ffi/
COPY tune-bridge/Cargo.toml tune-bridge/
RUN echo 'fn main() {}' > tune-server/src/main.rs && \
    echo 'fn main() {}' > tune-cli/src/main.rs && \
    touch tune-core/src/lib.rs tune-pyo3/src/lib.rs tune-server/src/lib.rs tune-ffi/src/lib.rs tune-bridge/src/lib.rs && \
    cargo build --release --package tune-server --no-default-features --features oaat 2>/dev/null || true && \
    rm -rf tune-core/src tune-pyo3/src tune-server/src tune-cli/src

# Build librespot (Spotify Connect) in parallel with deps cache
RUN cargo install librespot --no-default-features --features "alsa-backend" || true

# Build real source — clean dummy artifacts to force recompilation
COPY tune-core/ tune-core/
COPY tune-pyo3/ tune-pyo3/
COPY tune-server/ tune-server/
COPY tune-cli/ tune-cli/
COPY tune-ffi/ tune-ffi/
COPY tune-bridge/ tune-bridge/
RUN rm -rf target/release/.fingerprint/tune-* target/release/deps/tune_* target/release/deps/libtune_* target/release/tune-server && \
    cargo build --release --package tune-server --no-default-features --features oaat && \
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
COPY web/ /app/web/

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
