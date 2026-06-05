#!/usr/bin/env bash
# bump-all.sh — single command to bump the Tune version across every repo.
#
# Usage:
#   scripts/bump-all.sh 0.8.45
#
# Files touched (4):
#   1. tune-server-rust/Cargo.toml               — Rust workspace version
#   2. tune-web-client/package.json               — Svelte SPA
#   3. tune-server-flutter/pubspec.yaml           — Flutter (iOS + Android)
#   4. tune-server-ipados/Tune/project.yml        — SwiftUI (iPadOS / iOS / macOS)
#                                                   (also bumps CURRENT_PROJECT_VERSION +1)
#
# Does NOT commit, tag, or push — release decisions stay manual.
# Run `git diff` after to review.

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <version>   (e.g. $0 0.8.45)" >&2
    exit 2
fi

VERSION="$1"
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: version must be X.Y.Z (got: $VERSION)" >&2
    exit 2
fi

# Pre-flight: cargo fmt check to avoid CI Format failures on tag
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DEV="${TUNE_DEV_DIR:-$(cd "$RUST_DIR/.." && pwd)}"

if command -v cargo &>/dev/null; then
    if ! (cd "$RUST_DIR" && cargo fmt -- --check >/dev/null 2>&1); then
        echo "  Auto-formatting Rust code..." >&2
        (cd "$RUST_DIR" && cargo fmt)
        echo "  cargo fmt applied — will be included in the bump commit."
    fi
fi

CARGO="$RUST_DIR/Cargo.toml"
WEB="$DEV/tune-web-client/package.json"
FLUTTER="$DEV/tune-server-flutter/pubspec.yaml"
IPAD="$DEV/tune-server-ipados/Tune/project.yml"

for f in "$CARGO" "$WEB" "$FLUTTER" "$IPAD"; do
    [ -f "$f" ] || { echo "Error: missing $f" >&2; exit 1; }
done

# 1. Cargo.toml workspace — version = "X.Y.Z"
sed -i.bak -E 's/^version = "[0-9]+\.[0-9]+\.[0-9]+"/version = "'"$VERSION"'"/' "$CARGO" && rm "$CARGO.bak"

# 2. package.json — "version": "X.Y.Z"
sed -i.bak -E "s/\"version\": \"[0-9]+\\.[0-9]+\\.[0-9]+\"/\"version\": \"$VERSION\"/" "$WEB" && rm "$WEB.bak"

# 3. pubspec.yaml — version: X.Y.Z+N (preserve +N build number)
if grep -qE "^version: [0-9]+\\.[0-9]+\\.[0-9]+\\+[0-9]+" "$FLUTTER"; then
    sed -i.bak -E "s/^version: [0-9]+\\.[0-9]+\\.[0-9]+(\\+[0-9]+)/version: $VERSION\\1/" "$FLUTTER"
else
    sed -i.bak -E "s/^version: [0-9]+\\.[0-9]+\\.[0-9]+.*/version: $VERSION+1/" "$FLUTTER"
fi
rm "$FLUTTER.bak"

# 4. project.yml — MARKETING_VERSION + CURRENT_PROJECT_VERSION (++)
sed -i.bak -E "s/MARKETING_VERSION: \"[0-9]+\\.[0-9]+\\.[0-9]+\"/MARKETING_VERSION: \"$VERSION\"/g" "$IPAD"
CURRENT_BUILD=$(grep -oE "CURRENT_PROJECT_VERSION: [0-9]+" "$IPAD" | head -1 | awk '{print $2}')
NEXT_BUILD=$((CURRENT_BUILD + 1))
sed -i.bak -E "s/CURRENT_PROJECT_VERSION: [0-9]+/CURRENT_PROJECT_VERSION: $NEXT_BUILD/g" "$IPAD"
rm "$IPAD.bak"

echo
echo "Bumped Tune to v$VERSION (build $NEXT_BUILD for Apple targets)"
echo "  - $CARGO"
echo "  - $WEB"
echo "  - $FLUTTER"
echo "  - $IPAD"
echo
echo "Review with: git diff"
echo "Then commit + tag per repo."
