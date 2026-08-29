#!/usr/bin/env bash
# Build libtuneserver.so for Android (arm64 + armv7 + x86_64)
# Usage: ./build-android.sh [--release]
#
# Prerequisites:
#   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   Android NDK installed (detected automatically)
#
# Variables d'environnement :
#   ANDROID_NDK_HOME    NDK à utiliser (sinon : le plus récent installé)
#   TUNE_FLUTTER_JNI    dossier jniLibs de destination (sinon : dépôt frère
#                       tune-server-flutter). Indispensable pour construire
#                       depuis un worktree.
#
# Ce script ne « prévient » pas : il ÉCHOUE. Une bibliothèque qui ne porte pas
# la version en cours de construction n'est jamais recopiée (#1751) — c'est ce
# silence qui a envoyé les testeurs Android sur un moteur du 21 juillet
# (v0.8.354) sous une interface 0.9.76 pendant trois semaines et demie.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Find NDK
if [ -n "${ANDROID_NDK_HOME:-}" ]; then
    NDK="$ANDROID_NDK_HOME"
elif [ -d "$HOME/Library/Android/sdk/ndk" ]; then
    NDK="$(ls -d "$HOME/Library/Android/sdk/ndk"/*/ 2>/dev/null | sort -V | tail -1)"
    NDK="${NDK%/}"
elif [ -d "/usr/local/lib/android/sdk/ndk" ]; then
    NDK="$(ls -d "/usr/local/lib/android/sdk/ndk"/*/ 2>/dev/null | sort -V | tail -1)"
    NDK="${NDK%/}"
else
    echo "ERROR: Android NDK not found. Set ANDROID_NDK_HOME." >&2
    exit 1
fi

echo "Using NDK: $NDK"

# Find toolchain bin directory
HOST_TAG=""
if [ "$(uname)" = "Darwin" ]; then
    HOST_TAG="darwin-x86_64"
elif [ "$(uname)" = "Linux" ]; then
    HOST_TAG="linux-x86_64"
else
    echo "ERROR: Unsupported host OS" >&2
    exit 1
fi

TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin"
if [ ! -d "$TOOLCHAIN" ]; then
    echo "ERROR: Toolchain not found at $TOOLCHAIN" >&2
    exit 1
fi

export PATH="$TOOLCHAIN:$PATH"

# API level 24 = Android 7.0 (Flutter minimum)
API=24

# Build mode
PROFILE="release"
CARGO_FLAGS="--release"
if [ "${1:-}" != "--release" ]; then
    PROFILE="debug"
    CARGO_FLAGS=""
fi

cd "$PROJECT_ROOT"

TARGETS=(
    "aarch64-linux-android"
    "armv7-linux-androideabi"
    "x86_64-linux-android"
)

# `declare -A` exige bash 4 ; macOS livre 3.2.57 et le script mourait ici avant
# d'avoir rien construit. Deux fonctions `case` font le même travail partout.
linker_prefix() {
    case "$1" in
        aarch64-linux-android)    echo "aarch64-linux-android${API}-clang" ;;
        armv7-linux-androideabi)  echo "armv7a-linux-androideabi${API}-clang" ;;
        x86_64-linux-android)     echo "x86_64-linux-android${API}-clang" ;;
        *) echo "ERROR: unknown target $1" >&2; exit 1 ;;
    esac
}

abi_dir() {
    case "$1" in
        aarch64-linux-android)    echo "arm64-v8a" ;;
        armv7-linux-androideabi)  echo "armeabi-v7a" ;;
        x86_64-linux-android)     echo "x86_64" ;;
        *) echo "ERROR: unknown target $1" >&2; exit 1 ;;
    esac
}

# La version qui DOIT se retrouver dans chaque .so : celle du workspace, ou
# celle imposée par TUNE_VERSION (cf. tune_core::version()).
WORKSPACE_VERSION="$(grep -E '^version[[:space:]]*=' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed -e 's/.*"\(.*\)".*/\1/')"
EXPECTED_VERSION="${TUNE_VERSION:-$WORKSPACE_VERSION}"
if [ -z "$EXPECTED_VERSION" ]; then
    echo "ERROR: version illisible dans $PROJECT_ROOT/Cargo.toml" >&2
    exit 1
fi
echo "Version attendue dans les bibliothèques : $EXPECTED_VERSION"

# Destination jniLibs. L'ancien chemin était relatif à tune-ffi/ et pointait donc
# vers `tune-server-rust/tune-server-flutter/…` — un dossier qui n'existe pas,
# que `mkdir -p` créait en silence : les .so partaient dans le vide, et le dépôt
# Flutter gardait les siens. On résout depuis la racine du dépôt et on exige que
# la destination existe déjà.
FLUTTER_JNI="${TUNE_FLUTTER_JNI:-$PROJECT_ROOT/../tune-server-flutter/android/app/src/main/jniLibs}"
if [ ! -d "$FLUTTER_JNI" ]; then
    echo "ERROR: destination jniLibs introuvable : $FLUTTER_JNI" >&2
    echo "       (dépôt Flutter absent, ou construction depuis un worktree :" >&2
    echo "        renseignez TUNE_FLUTTER_JNI)" >&2
    exit 1
fi
FLUTTER_JNI="$(cd "$FLUTTER_JNI" && pwd)"
echo "Destination jniLibs : $FLUTTER_JNI"

for TARGET in "${TARGETS[@]}"; do
    echo ""
    echo "=== Building for $TARGET ==="

    PREFIX="$(linker_prefix "$TARGET")"
    TARGET_UNDERSCORE="${TARGET//-/_}"

    export CC_${TARGET_UNDERSCORE}="$TOOLCHAIN/${PREFIX}"
    # NDK 28 ne livre plus les noms historiques (`arm-linux-androideabi-clang++`)
    # que cc-rs déduit pour armv7 : sans CXX_<target>, la cible armv7 échoue.
    export CXX_${TARGET_UNDERSCORE}="$TOOLCHAIN/${PREFIX}++"
    export AR_${TARGET_UNDERSCORE}="$TOOLCHAIN/llvm-ar"

    # Use environment variable for linker (more reliable than config.toml)
    export "CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER=$TOOLCHAIN/${PREFIX}"

    SO_PATH="target/$TARGET/$PROFILE/libtuneserver.so"
    rm -f "$SO_PATH"

    # Pas de `| tail` : il masquait le code de sortie de cargo et ne laissait
    # que trois lignes pour diagnostiquer. Le log complet est le seul canari
    # des débordements 32 bits, qui ne cassent que sur armv7.
    cargo build -p tune-ffi --target "$TARGET" $CARGO_FLAGS

    if [ ! -f "$SO_PATH" ]; then
        echo "ERROR: $TARGET — cargo n'a produit aucun $SO_PATH" >&2
        exit 1
    fi

    # Le verrou : une bibliothèque qui ne contient pas la version en cours de
    # construction vient d'un build antérieur. On ne la recopie pas.
    if ! LC_ALL=C grep -a -q -F "$EXPECTED_VERSION" "$SO_PATH"; then
        echo "ERROR: $TARGET — $SO_PATH ne contient pas la version $EXPECTED_VERSION." >&2
        echo "       Bibliothèque périmée : on refuse de la livrer (#1751)." >&2
        exit 1
    fi

    ABI="$(abi_dir "$TARGET")"
    DEST="$FLUTTER_JNI/$ABI"
    mkdir -p "$DEST"
    cp "$SO_PATH" "$DEST/libtuneserver.so"
    SIZE=$(du -h "$SO_PATH" | cut -f1)
    echo "  → $DEST/libtuneserver.so ($SIZE, v$EXPECTED_VERSION)"
done

echo ""
echo "=== Build complete ==="
ls -lh "$FLUTTER_JNI"/*/libtuneserver.so

# Le dépôt Flutter fige l'empreinte des .so embarqués et fait échouer tout APK
# construit sur des bibliothèques périmées. On la régénère ici pour qu'elle ne
# puisse pas être oubliée ; le script refuse lui aussi de tamponner un binaire
# qui ne porte pas la bonne version.
FLUTTER_ROOT="$(cd "$FLUTTER_JNI/../../../../.." 2>/dev/null && pwd || true)"
GUARD="$FLUTTER_ROOT/scripts/check-native-libs.sh"
if [ -n "$FLUTTER_ROOT" ] && [ -x "$GUARD" ]; then
    echo ""
    echo "=== Mise à jour de l'empreinte côté Flutter ==="
    "$GUARD" --update
else
    echo ""
    echo "NOTE: $GUARD absent — pensez à régénérer l'empreinte des .so." >&2
fi
