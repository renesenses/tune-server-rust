FROM rust:1-slim-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY tune-core/ tune-core/
COPY tune-pyo3/ tune-pyo3/
COPY tune-server/ tune-server/

RUN cargo build --release --package tune-server --no-default-features && \
    strip target/release/tune-server

FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/tune-server /app/tune-server
COPY web/ /app/web/

ENV TUNE_MUSIC_DIRS='["/music"]' \
    TUNE_DB_PATH=/data/tune_v2.db \
    TUNE_ARTWORK_CACHE=/data/artwork_cache \
    TUNE_WEB_DIR=/app/web \
    TUNE_API_PORT=9888 \
    TUNE_STREAM_PORT=9080 \
    TUNE_LOG=info \
    TUNE_AUTO_SCAN=true

EXPOSE 9888 9080

VOLUME ["/data", "/music"]

ENTRYPOINT ["/app/tune-server"]
