# Tune Server (Rust)

Multi-room music server written in Rust. Manages a local audio library with full-text search, streams from Tidal/Qobuz/Deezer/Spotify, outputs to DLNA renderers and Chromecast devices, and serves a web client for control.

## Quick Start

### From Source

```bash
cargo build --release --package tune-server

TUNE_PORT=8085 \
TUNE_MUSIC_DIRS='["/path/to/music"]' \
TUNE_AUTO_SCAN=true \
  ./target/release/tune-server
```

Open `http://localhost:8085` in a browser.

### Docker

```bash
docker run -d \
  --name tune-server \
  --network host \
  -v /path/to/music:/music:ro \
  -v tune-data:/data \
  -e TUNE_AUTO_SCAN=true \
  renesenses/tune:dev
```

### docker-compose

```bash
cp docker-compose.yml docker-compose.override.yml
# Edit docker-compose.override.yml to set your music path
docker compose up -d
```

## Configuration

Copy `tune.toml.example` to `tune.toml` and edit, or use environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `TUNE_PORT` | 8085 | HTTP port |
| `TUNE_DB_PATH` | tune.db | SQLite database path |
| `TUNE_MUSIC_DIRS` | [] | Music directories (JSON array or comma-separated) |
| `TUNE_AUTO_SCAN` | false | Scan library on startup |
| `TUNE_SCAN_IO_CONCURRENCY` | *auto* | Parallel tag reads during a scan. Auto-detected from the storage: **4** on a spinning disk, **32** otherwise. Set it only to override that guess — a slow NAS may want less, a high-latency share more. Clamped to 1..=256. |
| `TUNE_WEB_DIR` | web | Web client directory |
| `TUNE_ARTWORK_DIR` | artwork_cache | Cover art cache |
| `TUNE_INGEST_STAGING` | *(next to artwork cache)* | Where drag-and-dropped files are staged before import |
| `TUNE_LOG_LEVEL` | info | Log level |

## Architecture

```
tune-core/         Business logic (library, DB, streaming, outputs, discovery)
tune-server/       Axum HTTP server (385 route handlers, 30 modules)
```

### Key Features

- **Library**: Parallel file scanning (rayon), metadata extraction (lofty), FTS5 full-text search
- **Ingest**: File a new folder into the library — preview the destination paths from a naming
  template, move or copy, then a targeted scan. Every job is undoable
- **Streaming**: Tidal (OAuth + HiRes FLAC), Qobuz (signed URLs), Deezer (ARL), Spotify
- **Outputs**: DLNA/UPnP (AVTransport SOAP), Chromecast (rust_cast), local (cpal)
- **Discovery**: SSDP multicast + mDNS, auto-zone creation
- **Playback**: Multi-zone, play queue, shuffle, repeat, crossfade, gapless
- **Playlists**: Local + smart playlists (JSON rules engine) + cross-service sync
- **Scrobbling**: Last.fm session auth + now playing
- **Real-time**: WebSocket events for all state changes

### Performance (42K tracks, 1.4 TB library)

| Metric | Value |
|--------|-------|
| Startup to listening | 4ms |
| Full library scan (44K files, 8 cores) | 2s metadata + 27s DB insert |
| API health check | 2ms |
| FTS5 search | 8-75ms |
| Binary size | 18 MB |
| RSS memory | 65-75 MB |

## API

All endpoints are under `/api/v1/`. Key routes:

```
GET  /api/v1/system/health
GET  /api/v1/system/version
GET  /api/v1/system/diagnostics

GET  /api/v1/library/tracks?limit=50&offset=0
GET  /api/v1/library/albums?limit=50
GET  /api/v1/library/artists?limit=50
GET  /api/v1/library/search?q=miles&limit=20

POST /api/v1/library/ingest/analyze     # read a folder's tags, guess the album
POST /api/v1/library/ingest/release-tracks  # a chosen edition's listing + pairing
POST /api/v1/library/ingest/plan        # preview destination paths + conflicts
POST /api/v1/library/ingest/apply       # move/copy into the library, then scan
POST /api/v1/library/ingest/jobs/{id}/undo

GET  /api/v1/zones
POST /api/v1/zones/{id}/play
POST /api/v1/zones/{id}/pause

GET  /api/v1/playlists
GET  /api/v1/streaming/services

WS   /ws
```

See `tune-server/src/routes/` for the full list of 385 handlers.

## Development

```bash
# Check compilation
cargo check --workspace

# Run tests
cargo test --workspace

# Run with debug logging
TUNE_LOG_LEVEL=debug cargo run --package tune-server

# Benchmarks against a running server
./bench.sh localhost:8085
```

## License

MIT
