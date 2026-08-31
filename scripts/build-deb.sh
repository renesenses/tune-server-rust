#!/usr/bin/env bash
#
# Construit le paquet Debian de tune-server À PARTIR d'une archive de release
# déjà publiée. Ce script ne compile RIEN : il réempaquette exactement les
# binaires que release.yml a produits sur ubuntu-22.04 (glibc 2.35), pour que
# le .deb et le .tar.gz d'une même version soient bit-pour-bit le même
# programme.
#
# Usage :
#   scripts/build-deb.sh --tarball <archive.tar.gz> --version <X.Y.Z> [--outdir <dir>]
#   scripts/build-deb.sh --tag v0.9.126 --arch amd64        # télécharge l'archive
#
# L'architecture est déduite du nom de l'archive (linux-x86_64 → amd64,
# linux-aarch64 → arm64) sauf si --arch la force.
#
# Dépendances : dpkg-deb seul (plus gh si l'on passe --tag). Le script tourne
# aussi bien sur Ubuntu que dans un conteneur Debian.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEB_SRC="${REPO_ROOT}/packaging/deb"

TARBALL=""
VERSION=""
ARCH=""
TAG=""
OUTDIR="$PWD"

die() {
    echo "build-deb: $*" >&2
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    --tarball) TARBALL="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --arch)    ARCH="$2";    shift 2 ;;
    --tag)     TAG="$2";     shift 2 ;;
    --outdir)  OUTDIR="$2";  shift 2 ;;
    -h | --help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) die "argument inconnu : $1" ;;
    esac
done

# --- Résolution de l'archive -------------------------------------------------

if [[ -z "$TARBALL" ]]; then
    [[ -n "$TAG" ]] || die "il faut --tarball ou --tag"
    case "${ARCH:-amd64}" in
    amd64) PLATFORM=linux-x86_64 ;;
    arm64) PLATFORM=linux-aarch64 ;;
    *) die "architecture inconnue pour --tag : ${ARCH}" ;;
    esac
    TARBALL="${OUTDIR}/tune-server-${TAG}-${PLATFORM}.tar.gz"
    if [[ ! -f "$TARBALL" ]]; then
        echo "--- téléchargement de $(basename "$TARBALL") ---"
        gh release download "$TAG" \
            --repo renesenses/tune-server-rust \
            --pattern "$(basename "$TARBALL")" \
            --dir "$OUTDIR"
    fi
    # `[[ ... ]] && VERSION=...` en dernière instruction du bloc renverrait 1
    # quand VERSION est déjà posée, et `set -e` avorterait le script.
    if [[ -z "$VERSION" ]]; then
        VERSION="${TAG#v}"
    fi
fi

[[ -f "$TARBALL" ]] || die "archive introuvable : $TARBALL"

# --- Architecture ------------------------------------------------------------

if [[ -z "$ARCH" ]]; then
    case "$(basename "$TARBALL")" in
    *linux-x86_64*)  ARCH=amd64 ;;
    *linux-aarch64*) ARCH=arm64 ;;
    *) die "impossible de déduire l'architecture de $(basename "$TARBALL") — passez --arch" ;;
    esac
fi

# --- Version -----------------------------------------------------------------
#
# Une version Debian doit commencer par un chiffre : `v0.9.126` est refusé par
# dpkg-deb, `0.9.126` passe. On tolère donc les deux en entrée.
if [[ -z "$VERSION" ]]; then
    VERSION="$(basename "$TARBALL" | sed -n 's/^tune-server-v\{0,1\}\([0-9][^-]*\)-linux.*/\1/p')"
fi
VERSION="${VERSION#v}"
[[ -n "$VERSION" ]] || die "version indéterminée — passez --version"
[[ "$VERSION" =~ ^[0-9] ]] || die "version Debian invalide (doit commencer par un chiffre) : $VERSION"

echo "--- tune-server ${VERSION} (${ARCH}) depuis $(basename "$TARBALL") ---"

# --- Arborescence ------------------------------------------------------------

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# mktemp crée en 0700 ; ce mode se retrouverait sur l'entrée « ./ » du paquet.
# dpkg ne l'applique pas à la racine du système, mais autant ne pas laisser
# traîner un 0700 dans les métadonnées du .deb.
chmod 0755 "$STAGE"

install -d "${STAGE}/DEBIAN"
install -d "${STAGE}/opt/tune"
install -d "${STAGE}/usr/bin"
install -d "${STAGE}/lib/systemd/system"
install -d "${STAGE}/etc/default"
install -d "${STAGE}/usr/share/doc/tune-server"

tar xzf "$TARBALL" -C "${STAGE}/opt/tune"

# Contrôle : l'archive doit contenir au moins le serveur et le client web,
# sinon on fabriquerait un paquet qui s'installe et ne démarre pas.
[[ -x "${STAGE}/opt/tune/tune-server" ]] || die "tune-server absent de l'archive"
[[ -f "${STAGE}/opt/tune/web/index.html" ]] || die "web/index.html absent de l'archive"

chmod 0755 "${STAGE}/opt/tune/tune-server"
for extra in airplay-daemon ffmpeg; do
    if [[ -f "${STAGE}/opt/tune/${extra}" ]]; then
        chmod 0755 "${STAGE}/opt/tune/${extra}"
    fi
done

# `tune-server` dans le PATH. current_exe() passe par /proc/self/exe, qui
# résout le lien : la mise à jour intégrée vise bien /opt/tune, pas /usr/bin.
ln -s /opt/tune/tune-server "${STAGE}/usr/bin/tune-server"

install -m 0644 "${DEB_SRC}/tune-server.service" "${STAGE}/lib/systemd/system/tune-server.service"
install -m 0644 "${DEB_SRC}/default"             "${STAGE}/etc/default/tune-server"
install -m 0644 "${DEB_SRC}/copyright"           "${STAGE}/usr/share/doc/tune-server/copyright"
install -m 0644 "${DEB_SRC}/README.Debian"       "${STAGE}/usr/share/doc/tune-server/README.Debian"

install -m 0755 "${DEB_SRC}/postinst" "${STAGE}/DEBIAN/postinst"
install -m 0755 "${DEB_SRC}/prerm"    "${STAGE}/DEBIAN/prerm"
install -m 0755 "${DEB_SRC}/postrm"   "${STAGE}/DEBIAN/postrm"

echo "/etc/default/tune-server" > "${STAGE}/DEBIAN/conffiles"

# --- control -----------------------------------------------------------------
#
# libc6 (>= 2.35) n'est pas décoratif : le ffmpeg embarqué référence des
# symboles GLIBC_2.35 (release.yml construit sur ubuntu-22.04). Sur Debian 11,
# apt refuse proprement l'installation au lieu de laisser l'utilisateur
# découvrir « GLIBC_2.35 not found » au premier lancement — c'est la raison
# d'être du paquet face à l'archive .tar.gz.
#
# libasound2 | libasound2t64 : Ubuntu 24.04 a renommé le paquet lors de la
# transition time_t 64 bits. L'alternative couvre les deux familles.
INSTALLED_KB="$(du -sk "${STAGE}" | cut -f1)"

cat > "${STAGE}/DEBIAN/control" <<EOF
Package: tune-server
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Mozaiklabs <contact@mozaiklabs.fr>
Installed-Size: ${INSTALLED_KB}
Depends: libc6 (>= 2.35), libgcc-s1, libstdc++6, libasound2 | libasound2t64, adduser
Recommends: avahi-daemon
Section: sound
Priority: optional
Homepage: https://github.com/renesenses/tune-server-rust
Description: Serveur de musique multi-room Tune
 Tune indexe une bibliothèque audio locale (FLAC, DSD, ALAC, MP3...), diffuse
 depuis Tidal, Qobuz, Deezer et Spotify, et envoie le son vers des appareils
 DLNA/UPnP, AirPlay, Chromecast, BluOS, Squeezebox ou un DAC USB local.
 .
 Le client web est servi par le serveur lui-même sur le port 8888.
 .
 Ce paquet installe le serveur comme service systemd sous un utilisateur
 dédié, et le démarre automatiquement.
EOF

# --- md5sums -----------------------------------------------------------------
# Ce que `debsums` lira. Les conffiles en sont exclus (ils changent par nature).
(
    cd "$STAGE"
    find . -type f ! -path './DEBIAN/*' ! -path './etc/*' -printf '%P\0' |
        sort -z | xargs -0 md5sum > DEBIAN/md5sums
)

# --- Construction ------------------------------------------------------------

mkdir -p "$OUTDIR"
DEB="${OUTDIR}/tune-server_${VERSION}_${ARCH}.deb"

# --root-owner-group : les fichiers du paquet appartiennent à root:root même
# si le script tourne sous un utilisateur ordinaire. C'est ce qui évite
# fakeroot, donc une dépendance de moins sur la machine de build.
dpkg-deb --root-owner-group --build "$STAGE" "$DEB"

echo ""
echo "--- $(basename "$DEB") — $(du -h "$DEB" | cut -f1) ---"
dpkg-deb --info "$DEB"
echo "$DEB"
