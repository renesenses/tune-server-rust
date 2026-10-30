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
    apt-get install -y --no-install-recommends libasound2-dev pkg-config cmake clang python3 && \
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
    cargo build --release --package tune-server --no-default-features --features oaat,cloud-relay,local-audio,bandcamp,dj,karaoke,plugins-wasm,audio-embedding,dst 2>/dev/null || true && \
    rm -rf tune-core/src tune-server/src tune-cli/src

# #6872: same source and dependency notices as the release archives.
COPY scripts/build-librespot.py scripts/
COPY packaging/librespot/ packaging/librespot/
RUN cargo install cargo-about --version 0.9.3 --locked && \
    python3 scripts/build-librespot.py --target "$(rustc -vV | sed -n 's/^host: //p')" \
      --output /opt/librespot --work /tmp/librespot-source

# Build airplay-daemon (AirPlay 2 sender, #700) — GPL-2.0 standalone subprocess
# binary from a pinned rev of our fork. Optional in this dev image: on failure,
# touch a placeholder so a local build still succeeds (AirPlay 2 then falls back
# to legacy AirPlay 1). The release path (Dockerfile.dist) requires it.
RUN cargo install --git https://github.com/renesenses/airplay2-rs \
      --rev a7d7ad6637507eaedd61de7461574adfb7b0a987 airplay-daemon --locked \
    || touch /usr/local/cargo/bin/airplay-daemon

# Build real source — clean dummy artifacts to force recompilation
COPY tune-core/ tune-core/
COPY tune-server/ tune-server/
COPY tune-cli/ tune-cli/
COPY tune-ffi/ tune-ffi/
COPY tune-bridge/ tune-bridge/
# Version affichée par le binaire : `tune_core::version()` lit TUNE_VERSION À LA
# COMPILATION. Sans lui, l'image se dit `X.Y.Z-dev` (jamais `X.Y.Z` tout court,
# qui passerait devant les RC du même numéro et bloquerait la mise à jour).
# Pour une image d'une version précise :
#   docker build --build-arg TUNE_VERSION=1.0.0-rc3 .
# Posé ICI, après le cache des dépendances : le changer ne recompile que Tune.
ARG TUNE_VERSION=
ENV TUNE_VERSION=${TUNE_VERSION}
# Même jeu de features que .github/workflows/docker.yml : une image construite
# depuis ce Dockerfile sans audio-embedding renvoyait available:false et l'entrée
# Ambiance disparaissait de l'UI sans aucun message (#19 de la revue 2026-08-15).
RUN rm -rf target/release/.fingerprint/tune-* target/release/deps/tune_* target/release/deps/libtune_* target/release/tune-server && \
    cargo build --release --package tune-server --no-default-features --features oaat,cloud-relay,local-audio,bandcamp,dj,karaoke,plugins-wasm,audio-embedding,dst && \
    strip target/release/tune-server

# ── Stage fpcalc : Chromaprint officiel, binaire précompilé ───────────
# Remplace le paquet Debian `libchromaprint-tools`, qui tirait les
# bibliothèques libav* du système. Version et sommes SHA-256 figées ici
# (release acoustid/chromaprint) ; toute mise à jour change les trois.
# x86_64 : binaire statique. arm64 : lié dynamiquement à la glibc, libstdc++
# et libgcc (présentes dans l'image d'exécution, voir plus bas).
FROM debian:bookworm-slim AS fpcalc
ARG TARGETARCH
ARG CHROMAPRINT_VERSION=1.6.2
ARG FPCALC_SHA256_AMD64=c968ad4a0821592a79b158fb2e1f46cbe0a22a0a1dbc0743c67d68e75d15d2bb
ARG FPCALC_SHA256_ARM64=3aa1498126ed272d805bc9d081c77965c9e1e1a0891bb112c13b56ef2ad47bad
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) arch=x86_64; sha="$FPCALC_SHA256_AMD64" ;; \
      arm64) arch=arm64;  sha="$FPCALC_SHA256_ARM64" ;; \
      *) echo "architecture non prise en charge : ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    f="chromaprint-fpcalc-${CHROMAPRINT_VERSION}-linux-${arch}"; \
    curl -fsSL -o /tmp/fpcalc.tar.gz \
      "https://github.com/acoustid/chromaprint/releases/download/v${CHROMAPRINT_VERSION}/${f}.tar.gz"; \
    echo "${sha}  /tmp/fpcalc.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/fpcalc.tar.gz -C /tmp; \
    install -m 755 "/tmp/${f}/fpcalc" /usr/local/bin/fpcalc

# ── Stage 2: Runtime ─────────────────────────────────────────────────
FROM debian:bookworm-slim

# `upgrade` : base à jour au moment de la construction.
# curl sert au HEALTHCHECK ; libstdc++6 et libgcc-s1 à fpcalc (arm64).
RUN apt-get update && \
    apt-get upgrade -y --no-install-recommends && \
    apt-get install -y --no-install-recommends \
      ca-certificates curl libasound2 python3-pip libstdc++6 libgcc-s1 && \
    pip3 install --break-system-packages yt-dlp && \
    rm -rf /var/lib/apt/lists/*

# fpcalc (Chromaprint) : empreinte sonore de la passe AcoustID (#4805), cherché
# dans le PATH. Sans lui, Bibliothèque › Enrichissement affiche « fpcalc n’est
# pas installé » (fil 2235).
COPY --from=fpcalc /usr/local/bin/fpcalc /usr/local/bin/fpcalc

# Create non-root user
# In `audio` for a USB DAC passed with `--device /dev/snd` (#5968). The
# entrypoint also keeps the HOST group of /dev/snd when it differs
# (Fedora: 63), and any `--group-add`.
RUN groupadd -g 1000 tune && \
    useradd -u 1000 -g tune -G audio -m -s /bin/false tune

WORKDIR /app

# Entrypoint: drops root, honours PUID/PGID (see the end of this file).
COPY packaging/docker/entrypoint.sh /usr/local/bin/tune-entrypoint
RUN chmod 755 /usr/local/bin/tune-entrypoint

COPY --from=builder /build/target/release/tune-server /app/tune-server
COPY --from=builder /opt/librespot/bin/librespot /usr/local/bin/librespot
COPY --from=builder /opt/librespot/LIBRESPOT-*.txt /usr/share/doc/librespot/
COPY --from=builder /usr/local/cargo/bin/airplay-daemon /usr/local/bin/airplay-daemon
COPY --from=web /web/dist/ /app/web/

# Ensure tune user can read the app but not write
RUN chown -R root:root /app && chmod -R 755 /app

# Create data + artwork_cache directories owned by tune
RUN mkdir -p /data/artwork_cache && chown -R tune:tune /data

# Plugins the user installs live under /data (owned by `tune`, kept across
# image upgrades). The ones shipped in /app/plugins stay read-only: uninstalling
# one disables and hides it (state in the database under /data) instead.
ENV TUNE_PORT=8888 \
    TUNE_DB_PATH=/data/tune.db \
    TUNE_LIBRESPOT=/usr/local/bin/librespot \
    TUNE_ARTWORK_DIR=/data/artwork_cache \
    TUNE_PLUGINS_USER_DIR=/data/plugins \
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

# No `USER tune`: the entrypoint starts as root only to drop to `tune`
# (uid/gid 1000) at once — or, when PUID/PGID are given, to re-number `tune`
# and give it /data first (packaging/docker/entrypoint.sh). The server itself
# never runs as root. `docker run --user ...` still works: the entrypoint then
# starts the server directly.
ENTRYPOINT ["/usr/local/bin/tune-entrypoint"]
