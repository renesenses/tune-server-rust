#!/bin/bash
# Build libtuneserver.so for Android (arm64 + armv7 + x86_64)
# Usage: ./build-android.sh [--release]
#
# Prerequisites:
#   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   Android NDK installed (detected automatically)

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

# Map target to NDK linker prefix
declare -A LINKER_PREFIX=(
    ["aarch64-linux-android"]="aarch64-linux-android${API}-clang"
    ["armv7-linux-androideabi"]="armv7a-linux-androideabi${API}-clang"
    ["x86_64-linux-android"]="x86_64-linux-android${API}-clang"
)

# Map target to Android ABI directory name
declare -A ABI_DIR=(
    ["aarch64-linux-android"]="arm64-v8a"
    ["armv7-linux-androideabi"]="armeabi-v7a"
    ["x86_64-linux-android"]="x86_64"
)

# Flutter jniLibs output directory
FLUTTER_JNI="$SCRIPT_DIR/../tune-server-flutter/android/app/src/main/jniLibs"

for TARGET in "${TARGETS[@]}"; do
    echo ""
    echo "=== Building for $TARGET ==="

    export CC_${TARGET//-/_}="$TOOLCHAIN/${LINKER_PREFIX[$TARGET]}"
    export AR_${TARGET//-/_}="$TOOLCHAIN/llvm-ar"
    export CARGO_TARGET_${TARGET//-/_}_LINKER="$TOOLCHAIN/${LINKER_PREFIX[$TARGET]}"

    # Use environment variable for linker (more reliable than config.toml)
    export "CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER=$TOOLCHAIN/${LINKER_PREFIX[$TARGET]}"

    cargo build -p tune-ffi --target "$TARGET" $CARGO_FLAGS 2>&1 | tail -3

    # Copy .so to Flutter jniLibs
    ABI="${ABI_DIR[$TARGET]}"
    SO_PATH="target/$TARGET/$PROFILE/libtuneserver.so"
    if [ -f "$SO_PATH" ]; then
        DEST="$FLUTTER_JNI/$ABI"
        mkdir -p "$DEST"
        cp "$SO_PATH" "$DEST/libtuneserver.so"
        SIZE=$(du -h "$SO_PATH" | cut -f1)
        echo "  → $DEST/libtuneserver.so ($SIZE)"
    else
        echo "  WARNING: $SO_PATH not found"
    fi
done

echo ""
echo "=== Build complete ==="
ls -lh "$FLUTTER_JNI"/*/libtuneserver.so 2>/dev/null || echo "No .so files found"
