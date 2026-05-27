# Tune v2.0.0 — Rust Migration Status

## Current Status

**Version**: v2.0.0-rc2 (workspace `0.1.0`)
**Engine**: Pure Rust (Axum + Tokio + rusqlite)
**LOC**: 28,422 Rust (across 3 crates)
**Routes**: 385 HTTP handlers across 30 route modules
**Tests**: 97 unit tests
**Binary**: 18 MB stripped (release, LTO)

The Python-to-Rust migration is **feature-complete** for the core server. The Rust binary runs in production on .18 alongside the Python server, serving 42,988 tracks / 3,788 albums / 2,032 artists from a 1.4 TB library.

---

## Architecture

### Workspace Structure

```
tune-server-rust/
  Cargo.toml              # workspace root (edition 2024)
  tune-core/              # Business logic library
    src/
      audio/              # Pipeline, WAV, formats
      db/                 # SQLite, 10 repos, models, migrations
      discovery/          # SSDP, mDNS, XML parser, device model
      http/               # Audio streamer (Range, proxy, ICY)
      outputs/            # DLNA, Chromecast, local (cpal)
      scanner/            # File walker (rayon), hasher, watcher, quality
      streaming/          # Tidal, Qobuz, Spotify, Deezer, YouTube (traits + impls)
      artwork.rs          # Embedded cover extraction (lofty)
      buffer.rs           # Async ring buffer
      metadata.rs         # Tag reader (lofty)
      orchestrator.rs     # Playback pipeline: service -> proxy -> output -> history
      playback.rs         # PlaybackManager (state machine)
      poller.rs           # Position poller for outputs
      scrobble.rs         # Last.fm scrobbling
  tune-pyo3/              # PyO3 bindings (legacy bridge, kept for compatibility)
  tune-server/            # Axum HTTP binary
    src/
      main.rs             # Entry point, auto-scan, file watcher, SSDP/mDNS init
      config.rs           # TuneConfig (TOML + env vars)
      state.rs            # AppState (DB, outputs, services, playback, streamer)
      error.rs            # Error types
      routes/             # 30 route modules (see below)
```

### Key Crates

| Crate | Purpose |
|-------|---------|
| `axum` 0.8 | HTTP framework (REST + WebSocket + multipart) |
| `tokio` 1.x | Async runtime |
| `rusqlite` 0.32 | SQLite with bundled engine, WAL, FTS5 |
| `lofty` 0.22 | Audio metadata reading (FLAC, MP3, AAC, etc.) |
| `reqwest` 0.12 | HTTP client (rustls, streaming) |
| `rayon` 1.10 | Parallel file scanning |
| `walkdir` 2 | Recursive directory traversal |
| `notify` 7 | File system watcher (inotify/FSEvents/kqueue) |
| `mdns-sd` 0.12 | mDNS service discovery (Chromecast, AirPlay) |
| `socket2` 0.5 | SSDP multicast |
| `quick-xml` 0.37 | UPnP XML parsing |
| `rust_cast` 0.21 | Chromecast protocol |
| `tower-http` 0.6 | CORS, gzip compression, tracing, static files |
| `cpal` 0.15 | Local audio output |

---

## Feature Parity

### Ported (production-ready)

- **All API routes** — 385 handlers vs ~335 in Python
- **Library** — Artists, albums, tracks, search (FTS5), stats, genres, ratings, history, dashboard
- **Streaming services** — Tidal (device OAuth + HiRes), Qobuz (signed URLs), Deezer (ARL), Spotify (stub)
- **DLNA output** — Full AVTransport SOAP (play/pause/stop/seek), RenderingControl (volume/mute), DIDL-Lite, DSD detection
- **Chromecast output** — mDNS discovery + rust_cast protocol
- **Last.fm scrobbling** — Session auth + scrobble + now playing
- **FTS5 search** — Full-text search on tracks, albums, artists with triggers
- **Playlist manager** — CRUD, track management, M3U import/export, duplicate, smart playlists (JSON rules engine)
- **Zone manager** — CRUD, volume/mute, auto-creation from discovered devices, stereo pairs
- **Playback orchestrator** — Full pipeline: service -> stream URL -> proxy -> output -> history
- **Audio streaming** — HTTP Range requests, proxy streaming, chunked transfer, ICY metadata
- **File scanner** — Parallel metadata extraction (rayon), audio hash, file watcher (notify)
- **Device discovery** — SSDP (M-SEARCH + NOTIFY + unicast probe), mDNS (Chromecast, AirPlay)
- **Snapcast, Sonos, Squeezebox, Spotify Connect** — Route stubs / basic integration
- **Metadata editing** — Track/album/artist tag writeback via lofty
- **Profiles, tags, radios, podcasts, plugins, export, network mounts, DJ, party mode**
- **WebSocket** — Real-time events (`/ws`)
- **Web client** — Static file serving with SPA fallback

### Not Yet Ported

- **AirPlay output** (RAOP protocol) — Complex binary protocol, may remain a gap
- **YouTube Music** — Unofficial API, stub only
- **PostgreSQL** — SQLite only (sqlx dependency present but unused)

---

## Performance Benchmarks

Measured on production server (.18): Intel 8-core, 16 GB RAM, 44,623 audio files, 1.4 TB library.

### Startup

| Metric | Python v0.7.129 | Rust v2.0 |
|--------|----------------|-----------|
| Server listening | ~26s | **4ms** |
| File enumeration (44,623 files) | ~8s | **300ms** |
| Parallel metadata scan (44,623 files) | ~120s | **2s** (rayon, all cores) |
| Full scan + DB insert (cold) | ~180s | **27s** |
| Full scan + DB insert (warm, skip unchanged) | N/A | **<1s** |

### API Latency (localhost, single request)

| Endpoint | Latency |
|----------|---------|
| `GET /api/v1/system/health` | 2ms |
| `GET /api/v1/zones` | 3ms |
| `GET /api/v1/library/artists?limit=50` | 4ms |
| `GET /api/v1/library/albums?limit=50` | 8ms |
| `GET /api/v1/library/search?q=coltrane&limit=20` | 8ms |
| `GET /api/v1/library/search?q=miles&limit=20` | 11ms |
| `GET /api/v1/library/tracks?limit=50` | 58ms |
| `GET /api/v1/library/search?q=love&limit=20` | 75ms |

### Resource Usage

| Metric | Python v0.7.129 | Rust v2.0 |
|--------|----------------|-----------|
| Binary size | 100+ MB (PyInstaller) | **18 MB** (stripped, LTO) |
| RSS memory (42K tracks) | 200 MB | **65-75 MB** |
| RSS memory peak (during scan) | 400-600 MB | **~95 MB** |
| CPU at idle | 6.4% | **3.0%** |
| Systemd memory peak | N/A | **57.9 MB** |

### Key Numbers

- **Startup to listening**: 4ms (vs 26s Python) — **6,500x faster**
- **Binary size**: 18 MB vs 100+ MB — **5.5x smaller**
- **Memory**: 65 MB vs 200 MB — **3x less**
- **Parallel scan**: 2s for 44K files on 8 cores — **60x faster** than Python single-threaded

---

## Route Modules (30)

| Module | Description |
|--------|-------------|
| `system` | Version, health, stats, config, scan, restart, diagnostics, logs, backup/restore, DB export, cleanup, update check |
| `library` | Artists/albums/tracks (paginated), count, filters, recent, genre tree, top-rated, ratings, rescan |
| `search` | Federated FTS5 search (library + radios + streaming services) |
| `playback` | Play/pause/resume/stop/next/previous/seek/volume/shuffle/repeat, EQ/DSP, crossfade, normalization, transfer, sleep timer, alarms |
| `zones` | CRUD, volume/mute/rename, groups, stereo pairs |
| `playlists` | CRUD, track management, duplicate, M3U export/import |
| `smart_playlists` | JSON rules engine, dynamic resolution |
| `streaming` | Service auth, search, browse, track URLs, featured, new releases |
| `profiles` | CRUD, favorites add/remove/list |
| `tags` | CRUD, tag/untag items |
| `metadata` | Track/album/artist tag editing with lofty writeback |
| `radios` | CRUD, search, favorites, alarms |
| `history` | Recent listens |
| `dashboard` | Top tracks/artists, genre breakdown, listening stats |
| `devices` | SSDP scan + DLNA auto-registration |
| `network` | SMB/NFS mount CRUD |
| `export` | CSV tracks/albums/artists |
| `podcasts` | Subscription CRUD |
| `plugins` | List/enable/disable |
| `ws` | WebSocket real-time events |
| `playlist_manager` | Cross-service playlist orchestrator |
| `zone_manager` | Advanced zone management |
| `snapcast` | Snapcast integration |
| `sonos` | Sonos integration |
| `squeezebox` | Squeezebox/LMS integration |
| `spotify_connect` | Spotify Connect integration |
| `dj` | DJ/auto-mix mode |
| `party` | Party mode (collaborative queue) |
| `peers` | Multi-server peer discovery |

---

## Database

- **Engine**: rusqlite 0.32 with bundled SQLite (WAL mode, foreign keys, busy_timeout)
- **FTS5**: Virtual tables + triggers on tracks, albums, artists
- **Migrations**: 12 versioned migrations (auto-applied at startup)
- **Repos**: 10 repositories (artist, album, track, playlist, play_queue, zone, profile, tag, radio, rating, history, settings)

---

## Build & Deploy

### Local Development

```bash
# Build
cargo build --release --package tune-server

# Run
TUNE_PORT=8085 TUNE_MUSIC_DIRS='["/path/to/music"]' TUNE_AUTO_SCAN=true \
  ./target/release/tune-server

# Run tests
cargo test --workspace
```

### Configuration

Configuration via `tune.toml` or environment variables (env vars take precedence):

| Env Var | Default | Description |
|---------|---------|-------------|
| `TUNE_PORT` | 8085 | HTTP port |
| `TUNE_DB_PATH` | tune.db | SQLite database path |
| `TUNE_WEB_DIR` | web | Web client directory |
| `TUNE_ARTWORK_DIR` | artwork_cache | Artwork cache directory |
| `TUNE_MUSIC_DIRS` | [] | JSON array or comma-separated paths |
| `TUNE_AUTO_SCAN` | false | Scan music dirs on startup |
| `TUNE_LOG_LEVEL` | info | Log level (trace/debug/info/warn/error) |
| `QOBUZ_APP_ID` | | Qobuz API app ID |
| `QOBUZ_APP_SECRET` | | Qobuz API app secret |

### Deploy to Server

```bash
# Cross-compile for Linux x86_64
cargo build --release --package tune-server --target x86_64-unknown-linux-gnu

# Copy to server
scp target/x86_64-unknown-linux-gnu/release/tune-server user@server:/opt/tune-server-rust/

# Restart
ssh user@server 'sudo systemctl restart tune-rust'
```

### Systemd Service

```ini
[Unit]
Description=Tune Server v2 (Rust)
After=network.target

[Service]
Type=simple
User=bertrand
WorkingDirectory=/opt/tune-server-rust
ExecStart=/opt/tune-server-rust/tune-server
Restart=always
RestartSec=3
Environment=TUNE_PORT=8086
Environment=TUNE_DB_PATH=/opt/tune-server-rust/tune.db
Environment=TUNE_WEB_DIR=/opt/tune-server-rust/web
Environment=TUNE_ARTWORK_DIR=/opt/tune-server-rust/artwork_cache
Environment=TUNE_AUTO_SCAN=true
Environment=TUNE_MUSIC_DIRS=["/data/music","/data/recordings"]
Environment=TUNE_LOG_LEVEL=info

[Install]
WantedBy=multi-user.target
```

---

## Docker

### Image: `renesenses/tune:dev`

```bash
# Build
docker build -t renesenses/tune:dev .

# Run
docker run -d \
  --name tune-server \
  --network host \
  -v /path/to/music:/music:ro \
  -v tune-data:/data \
  -e TUNE_AUTO_SCAN=true \
  renesenses/tune:dev
```

### docker-compose

```yaml
services:
  tune:
    image: renesenses/tune:dev
    container_name: tune-server
    restart: unless-stopped
    network_mode: host
    volumes:
      - tune-data:/data
      - /path/to/music:/music:ro
    environment:
      - TUNE_PORT=8085
      - TUNE_DB_PATH=/data/tune.db
      - TUNE_ARTWORK_DIR=/data/artwork_cache
      - TUNE_AUTO_SCAN=true
      - TUNE_MUSIC_DIRS=/music

volumes:
  tune-data:
```

The Docker image uses a multi-stage build: Rust compilation on `rust:1-slim-bookworm`, runtime on `debian:bookworm-slim` with FFmpeg and ca-certificates.

---

## CI/CD

Three GitHub Actions workflows:

- **ci.yml** — Build + test on push/PR (cargo check, cargo test, clippy)
- **docker.yml** — Build and push Docker image to Docker Hub
- **release.yml** — Cross-compile release binaries (Linux x86_64/aarch64, macOS)

---

## Migration History

The original plan described 8 phases over ~20 months. In practice, the migration was completed in roughly 4 weeks of intensive development (May 2026), going directly from Phase 0 (tooling) to a feature-complete server. The PyO3 bridge (`tune-pyo3`) was built but the decision was made early to port everything to pure Rust rather than maintaining a hybrid Python/Rust architecture.

| Milestone | Date | Description |
|-----------|------|-------------|
| Phase 0: Workspace created | 2026-05-12 | Cargo workspace, CI, PyO3 scaffolding |
| Phases 1-4: Core modules | 2026-05-14 | Metadata, scanner, discovery ported |
| Phase 5: Database + streamer | 2026-05-16 | rusqlite, all repos, HTTP streamer |
| Phase 6: Full API | 2026-05-19 | 147 initial endpoints on Axum |
| Phase 7: Connectors + outputs | 2026-05-19 | Tidal, Qobuz, DLNA, Chromecast |
| Phase 8: Production deploy | 2026-05-20 | Deployed on .18, running alongside Python |
| 385 routes milestone | 2026-05-26 | Full feature parity + extras |
