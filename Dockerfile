FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache dependencies: copy manifests and build with dummy sources
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p tune-core/src tune-pyo3/src tune-server/src/routes
COPY tune-core/Cargo.toml tune-core/
COPY tune-pyo3/Cargo.toml tune-pyo3/
COPY tune-server/Cargo.toml tune-server/
RUN echo 'fn main() {}' > tune-server/src/main.rs && \
    touch tune-core/src/lib.rs tune-pyo3/src/lib.rs tune-server/src/lib.rs && \
    cargo build --release --package tune-server --no-default-features 2>/dev/null || true && \
    rm -rf tune-core/src tune-pyo3/src tune-server/src

# Build real source
COPY tune-core/ tune-core/
COPY tune-pyo3/ tune-pyo3/
COPY tune-server/ tune-server/
RUN cargo build --release --package tune-server --no-default-features && \
    strip target/release/tune-server

FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/tune-server /app/tune-server
COPY web/ /app/web/

ENV TUNE_MUSIC_DIRS='["/music"]' \
    TUNE_DB_PATH=/data/tune_v2.db \
    TUNE_ARTWORK_CACHE=/data/artwork_cache \
    TUNE_WEB_DIR=/app/web \
    TUNE_PORT=8888 \
    TUNE_LOG=info \
    TUNE_AUTO_SCAN=true

EXPOSE 8888

VOLUME ["/data", "/music"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8888/system/stats || exit 1

ENTRYPOINT ["/app/tune-server"]
