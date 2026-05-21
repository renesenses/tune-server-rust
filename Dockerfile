FROM rust:1.87-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libasound2-dev && rm -rf /var/lib/apt/lists/*

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

COPY --from=builder /build/target/release/tune-server /usr/local/bin/tune-server

ENV TUNE_PORT=8085
ENV TUNE_DB_PATH=/data/tune.db
ENV TUNE_WEB_DIR=/app/web
ENV TUNE_ARTWORK_DIR=/data/artwork_cache
ENV TUNE_AUTO_SCAN=true

EXPOSE 8085

VOLUME ["/data", "/music"]

ENTRYPOINT ["tune-server"]
