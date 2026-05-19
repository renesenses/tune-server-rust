# Tune v2.0.0 — Plan de migration incrémentale Python → Rust

## Contexte

Tune Server est un serveur audio multi-room de 62 266 lignes Python (195 fichiers, 20 modules). Les limites du Python (GIL, mémoire 400-600 MB, binaires PyInstaller 100+ MB, démarrage 3-5s) freinent la croissance. Rust résout tous ces problèmes tout en gardant la sécurité mémoire. La migration doit être **incrémentale** (développeur solo, ~20h/semaine) via PyO3 comme pont, sans régression fonctionnelle.

**Objectif v2.0.0** : binaire Rust pur, 15-20 MB, démarrage <0.5s, 50-80 MB RSS, 5-10x plus rapide en scan.

---

## Phase 0 — Tooling Foundation (v0.8.0) — 1-2 semaines

**But** : Infrastructure Rust sans toucher au Python.

- Cargo workspace : `tune-core/` (lib), `tune-pyo3/` (PyO3 bindings), `tune-server/` (binaire final, vide)
- CI cross-compilation : x86_64/aarch64 Linux, macOS, Windows
- `maturin` intégré dans `release.yml` pour produire des wheels
- Fonction triviale `tune_native.version()` pour valider le pont PyO3
- **Kill switch** : `try/except ImportError` sur tous les imports `tune_native`

**Crates** : `pyo3`, `maturin`
**LOC Rust** : ~200 | **Remplace** : 0 Python

---

## Phase 1 — Metadata Reader + Audio Buffer (v1.0.0) — 4-6 semaines

**But** : Remplacer les deux modules les plus découplés et CPU-intensifs.

**Modules remplacés** :
- `library/metadata_reader.py` (650 LOC) — parsing tags audio via mutagen
- `audio/buffer.py` (68 LOC) — ring buffer async

**Implémentation** :
- `tune_native.read_metadata(path) -> dict` via `lofty` (pure Rust, 4x plus rapide que mutagen)
- `tune_native.AsyncRingBuffer` via `tokio::sync::mpsc` bounded channel
- Shim Python : `TUNE_METADATA_ENGINE=rust|python` pour dual-run validation
- Endpoint `POST /system/validate-metadata` pour comparer les deux engines

**Crates** : `lofty`, `tokio`, `pyo3-asyncio`
**LOC Rust** : ~1 200 | **Remplace** : 718 Python
**Gains** : scan 3-5x plus rapide, -30% mémoire pendant scan

---

## Phase 2 — Audio Pipeline Core (v1.1.0) — 6-8 semaines

**But** : Rust gère le cycle de vie FFmpeg, le piping I/O, et les buffers. FFmpeg reste en subprocess.

**Modules remplacés** :
- `audio/pipeline.py` (256), `decoder.py` (276), `encoder.py` (116), `resampler.py` (123)
- `audio/formats.py` (230), `analyzer.py` (239), `mixer.py` (104)

**Implémentation** :
- `tune_native.AudioPipeline` : decode → resample → encode, exposition du buffer Rust
- `tune_native.FFmpegDecoder` : subprocess async via `tokio::process`
- WAV header generation pure Rust (remplace `struct.pack`)
- Élimine la dépendance numpy (~15 MB du binaire)

**Crates** : `tokio::process`, `bytes`, `symphonia` (optionnel), `hound`
**LOC Rust** : ~2 400 | **Remplace** : 1 412 Python
**Gains** : -30-50% latence streaming, -15 MB binaire (numpy éliminé), parallélisme réel multi-streams

---

## Phase 3 — Discovery Layer (v1.2.0) — 5-7 semaines ✅ CORE DONE (2026-05-19)

**But** : SSDP multicast, mDNS, et parsing XML device en Rust natif.

**Modules remplacés** :
- `discovery/ssdp.py` (852), `mdns.py` (~200), `manager.py` (~250 partiel)
- `discovery/openhome.py` (~300), `bluos.py` (~200), `cast.py` (~200), `squeezebox.py` (~200)

**Implémentation** :
- ✅ `tune_native.RustSsdpScanner` : M-SEARCH + NOTIFY + unicast probe + grace period
- ✅ `tune_native.RustMdnsScanner` : `_airplay._tcp`, `_googlecast._tcp`, `_musc._tcp`, `_cli._tcp`, `_tune-server._tcp`
- ✅ XML device description parser via `quick-xml` (10x plus rapide qu'ElementTree)
- ✅ `DiscoveredDevice` model + dedup by host with priority + alternatives
- ✅ PyO3 bindings : `RustSsdpScanner` + `RustMdnsScanner` classes avec `poll_event()`
- ✅ Python integration shim : `tune_server/discovery/rust_discovery.py`
- ✅ Intégration dans `DiscoveryManager` (toggle via `TUNE_DISCOVERY_ENGINE`)
- ✅ Diagnostics endpoint `/system/diagnostics` expose `rust_engines` status
- ⬜ Benchmark comparison Python vs Rust discovery

**Fichiers Rust** :
- `tune-core/src/discovery/device.rs` — DiscoveredDevice + OutputType + dedup
- `tune-core/src/discovery/ssdp.rs` — SSDP M-SEARCH + grace period + unicast probe
- `tune-core/src/discovery/mdns.rs` — mDNS multi-service browsing (mdns-sd)
- `tune-core/src/discovery/xml_parser.rs` — UPnP device description XML parsing
- `tune-pyo3/src/discovery_wrapper.rs` — PyO3 bindings

**Crates** : `mdns-sd`, `quick-xml`, `reqwest`, `socket2`, `uuid`
**LOC Rust** : ~1 100 | **Remplace** : 2 202 Python (discovery core)
**Gains** : discovery 2-3x plus rapide, -50% mémoire discovery, -0.5s startup

---

## Phase 4 — Library Scanner + File Watcher (v1.3.0) — 4-5 semaines ✅ CORE DONE (2026-05-19)

**But** : File walk + metadata reading parallélisés sur tous les cores CPU.

**Modules remplacés** :
- `library/scanner.py` (585), `watcher.py` (169), `artwork.py` (~234 partiel)

**Implémentation** :
- ✅ `tune_native.list_audio_files(dirs)` : `walkdir` enumération rapide
- ✅ `tune_native.scan_directories(dirs, with_hash)` : `walkdir` + `rayon` parallélisme complet
- ✅ `tune_native.RustFileWatcher` : `notify` (inotify/FSEvents/kqueue natif) avec debounce
- ✅ `tune_native.audio_hash(path)` : MD5 64KB at 25% offset
- ✅ `tune_native.same_quality_tier()` + `tune_native.quality_suffix_fn()`
- ✅ PyO3 bindings : 6 fonctions + `RustFileWatcher` class
- ✅ Python integration : `rust_scanner.py` shim + `scanner.py` hot path replacement
- ⬜ Benchmark comparison Python vs Rust scan speed
- ⬜ Artwork extraction via lofty (cover art embedded dans les tags)

**Fichiers Rust** :
- `tune-core/src/scanner/walker.rs` — Parallel file walk + metadata via rayon
- `tune-core/src/scanner/hasher.rs` — Audio hash MD5
- `tune-core/src/scanner/watcher.rs` — File watcher via notify crate
- `tune-core/src/scanner/quality.rs` — Quality tier helpers
- `tune-pyo3/src/scanner_wrapper.rs` — PyO3 bindings

**Crates** : `walkdir`, `rayon`, `notify`, `md-5`
**LOC Rust** : ~750 | **Remplace** : 988 Python (scanner hot paths)
**Gains** : scan 5-10x plus rapide (parallèle sur tous les cores, pas de GIL), -60% mémoire scan

---

## Phase 5 — Database + HTTP Streamer (v1.5.0) — 10-14 semaines — 🔧 IN PROGRESS

**But** : Couche données et streaming HTTP en Rust. Plus grande phase.

**Modules remplacés** :
- `db/engine.py` (773), `repository.py` (2 015), `sa_engine.py` (498), `sa_repository.py` (1 773)
- `outputs/http_streamer.py` (613)
- `models.py` (1 028 partiel)

**Implémentation** :
- ✅ `SqliteDb` : rusqlite wrapper (WAL, foreign keys, busy_timeout, PRAGMA)
- ✅ `ArtistRepo` : CRUD, get_or_create, search (LIKE COLLATE NOCASE)
- ✅ `AlbumRepo` : CRUD, get_or_create, delete_orphans, update_track_count, search
- ✅ `TrackRepo` : CRUD, get_all_paths, get_multiple (order-preserving), search, deduplicate
- ✅ Models : Artist, Album, Track, TrackCredit structs
- ✅ PyO3 `RustDatabase` class exposing key operations
- ⬜ PlaylistRepo, PlayQueueRepo, ZoneRepo, RadioStationRepo
- ⬜ FTS5 virtual tables + triggers
- ⬜ PostgreSQL backend (sqlx)
- ⬜ HTTP audio streamer (Axum)
- ⬜ Schema migrations

**Fichiers Rust** :
- `tune-core/src/db/sqlite.rs` — SQLite wrapper + core schema
- `tune-core/src/db/models.rs` — Artist, Album, Track, TrackCredit
- `tune-core/src/db/artist_repo.rs` — ArtistRepo (8 methods)
- `tune-core/src/db/album_repo.rs` — AlbumRepo (12 methods)
- `tune-core/src/db/track_repo.rs` — TrackRepo (14 methods)
- `tune-pyo3/src/db_wrapper.rs` — RustDatabase PyO3 class

**Crates** : `rusqlite` (bundled SQLite)
**LOC Rust** : ~1 330 (de ~6 000 cible) | **Remplace** : 6 697 Python
**Gains** : queries 2-5x plus rapides, -50% mémoire/stream, -20 MB binaire

---

## Phase 6 — API Layer (v1.8.0) — 16-22 semaines

**But** : FastAPI → Axum. Le binaire Rust sert tous les endpoints HTTP. Python sort du hot path.

**Modules remplacés** :
- `api/routes/*` (17 738 LOC, 42 fichiers), `api/deps.py`, `api/websocket.py`
- `app.py` (1 462), `config.py` (289), `event_bus.py` (172)

**Implémentation** :
- Chaque route FastAPI → handler Axum (contrat API byte-identical pour les clients Flutter/Swift/Web)
- WebSocket via `tokio-tungstenite` avec pattern-based filtering
- Event bus via `tokio::sync::broadcast`
- Config via `figment` (même variables `TUNE_*`)
- Logging via `tracing` (remplace `structlog`)

**Crates** : `axum`, `tower-http`, `tokio-tungstenite`, `figment`, `tracing`, `clap`
**LOC Rust** : ~25 000 | **Remplace** : 19 661 Python
**Gains** : startup <0.5s, RSS 50-80 MB, latence API 2-5x

---

## Phase 7 — Streaming Connectors + Outputs (v1.9.0) — 14-18 semaines

**But** : Migrer les connecteurs streaming (Tidal, Qobuz, Spotify, Deezer, YouTube, Amazon) et les outputs (DLNA, AirPlay, Chromecast, local, BluOS, OpenHome, Squeezebox).

**Modules remplacés** :
- `streaming/*.py` (4 993), `outputs/*.py` (5 577)
- `playback/*.py` (2 669), `zones/*.py` (1 616)

**Implémentation** :
- Trait `StreamingService` : chaque connecteur = struct Rust + `reqwest`
- Trait `OutputTarget` : DLNA (`rupnp`), local (`cpal`), Chromecast
- AirPlay : possiblement garde un shim subprocess vers `pyatv` (RAOP complexe)
- Player state machine Rust (Stopped → Playing → Paused)

**Crates** : `reqwest`, `cpal`, `rupnp`, `oauth2`, `aes-gcm`
**LOC Rust** : ~15 000 | **Remplace** : 14 855 Python

---

## Phase 8 — Polish + Release (v2.0.0) — 4-6 semaines

**But** : Binaire Rust pur. Python supprimé. Distribution simplifiée.

- Supprimer PyO3, maturin, toutes les dépendances Python
- Binaire statique cross-compilé (musl sur Linux)
- Docker : `FROM scratch` + binaire + FFmpeg + web assets
- Homebrew/DMG/NSIS mis à jour
- Profiling performance (flamegraph)

**LOC Rust** : ~2 000 | **Remplace** : packaging

---

## Timeline

| Phase | Version | Durée | Cumulé | Rust LOC | Python remplacé |
|-------|---------|-------|--------|----------|-----------------|
| 0 Tooling | v0.8.0 | 1-2 sem | 2 sem | 200 | 0 |
| 1 Metadata | v1.0.0 | 4-6 sem | 8 sem | 1 200 | 718 |
| 2 Audio | v1.1.0 | 6-8 sem | 16 sem | 2 400 | 1 412 |
| 3 Discovery | v1.2.0 | 5-7 sem | 23 sem | 2 000 | 2 202 |
| 4 Scanner | v1.3.0 | 4-5 sem | 28 sem | 1 500 | 988 |
| 5 DB+Streamer | v1.5.0 | 10-14 sem | 42 sem | 6 000 | 6 697 |
| 6 API | v1.8.0 | 16-22 sem | 64 sem | 25 000 | 19 661 |
| 7 Connectors | v1.9.0 | 14-18 sem | 82 sem | 15 000 | 14 855 |
| 8 Polish | v2.0.0 | 4-6 sem | 88 sem | 2 000 | 0 |
| **Total** | | | **~20 mois** | **~55 300** | **~46 533** |

---

## Métriques cibles v2.0.0

| Métrique | Python (actuel) | Rust (cible) | Gain |
|----------|----------------|--------------|------|
| Binaire | 100-130 MB | 15-20 MB | **6-7x** |
| Mémoire RSS (50K tracks) | 400-600 MB | 50-80 MB | **6-8x** |
| Démarrage | 3-5 s | <0.5 s | **7-10x** |
| Scan bibliothèque | ~60s / 10K tracks | ~6-10s / 10K tracks | **5-10x** |
| Streams concurrents | ~10-20 | 100+ | **5-10x** |
| Latence API | ~5-15 ms | ~1-3 ms | **3-5x** |

---

## Kill Switch

**Phases 0-5** : réversible en <1h
- Supprimer `from tune_native import`, fallback Python automatique
- Supprimer le workspace Cargo
- Résultat : retour au Python pur, zéro régression

**Phase 6+** : réversible mais coûteux
- `git revert` des commits API
- Réactiver le pont PyO3 pour les phases 1-5

**Point de décision** : si une phase prend >2x le temps estimé, pause et shipping du hybride.

---

## Structure Cargo

```
tune-server-linux/
  Cargo.toml                # workspace
  tune-core/src/            # Business logic
    audio/ discovery/ library/ db/ streaming/
    outputs/ playback/ zones/ models.rs config.rs event_bus.rs
  tune-pyo3/src/lib.rs      # PyO3 bindings (phases 1-5)
  tune-server/src/main.rs   # Axum binary (phase 6+)
  tune_server/              # Python (rétrécit progressivement)
```

---

## Fichiers critiques (première implémentation)

- `tune_server/library/metadata_reader.py` — Phase 1, premier module migré
- `tune_server/audio/pipeline.py` — Phase 2
- `tune_server/audio/buffer.py` — Phase 1
- `tune_server/discovery/ssdp.py` — Phase 3
- `tune_server/outputs/http_streamer.py` — Phase 5
- `tune_server/db/repository.py` — Phase 5
- `tune_server/api/routes/*` — Phase 6

## Vérification

Chaque phase est validée par :
1. Tests existants Python passent avec le backend Rust
2. Dual-run : Python et Rust produisent des résultats identiques
3. Benchmark : mesure des gains de performance avant/après
4. Test d'intégration : clients Flutter/Swift/Web fonctionnent sans modification
