#!/usr/bin/env bash
# bump-all.sh — bump the Tune version across the repos that actually ship.
#
# Relocated here from tune-server-linux (2026-08-08). That repo is the old
# PYTHON server: dead, frozen, to be forgotten. Keeping the release tooling
# inside it meant the release flow still had a foot in a dead repo.
#
# Each ecosystem keeps its own version file — there is no portable way to
# derive them from one place without invasive build-tool changes. So this
# script is the source of truth: it validates the version and rewrites every
# file in lock-step.
#
# Usage:
#   scripts/bump-all.sh 0.9.55              # server + web  (the release pair)
#   scripts/bump-all.sh 0.9.55 --clients    # + Flutter and iPadOS/iOS/macOS
#
# Does NOT commit, tag, or push — release decisions stay manual.
# Review with `git diff` in each repo afterwards.
#
# ⚠️ ORDER MATTERS, and this script does not enforce it. The web bump must be
# committed AND pushed to tune-web-client `main` BEFORE the server tag is
# pushed: release.yml has a "Verify web-client version matches release tag"
# gate that fails the whole release otherwise. See the `reference_release_doctrine`
# memory for the full pipeline.

set -euo pipefail

VERSION=""
WITH_CLIENTS=0
for arg in "$@"; do
    case "$arg" in
        --clients) WITH_CLIENTS=1 ;;
        -*) echo "Error: unknown flag $arg" >&2; exit 2 ;;
        *) VERSION="$arg" ;;
    esac
done

if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version> [--clients]   (e.g. $0 0.9.55)" >&2
    exit 2
fi
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: version must be X.Y.Z (got: $VERSION)" >&2
    exit 2
fi

DEV="${TUNE_DEV_DIR:-$HOME/DEV}"
RUST_DIR="$DEV/tune-server-rust"
CARGO="$RUST_DIR/Cargo.toml"
WEB="$DEV/tune-web-client/package.json"
FLUTTER="$DEV/tune-server-flutter/pubspec.yaml"
IPAD="$DEV/tune-server-ipados/Tune/project.yml"

REQUIRED=("$CARGO" "$WEB")
[ "$WITH_CLIENTS" -eq 1 ] && REQUIRED+=("$FLUTTER" "$IPAD")
for f in "${REQUIRED[@]}"; do
    [ -f "$f" ] || { echo "Error: missing $f" >&2; exit 1; }
done

# Pre-flight: keep the Rust tree formatted so the tag doesn't trip CI's format
# check. Non-fatal: a cargo hiccup here must not abort a release bump — worst
# case CI's format job tells you, which is exactly what it is there for.
if [ -d "$RUST_DIR" ] && command -v cargo &>/dev/null; then
    if ! (cd "$RUST_DIR" && cargo fmt -- --check >/dev/null 2>&1); then
        echo "  Auto-formatting Rust code..." >&2
        if (cd "$RUST_DIR" && cargo fmt >/dev/null 2>&1); then
            echo "  cargo fmt applied — include it in the bump commit."
        else
            echo "  ! cargo fmt failed — check formatting by hand before tagging." >&2
        fi
    fi
fi

# 1. Cargo.toml workspace — version = "X.Y.Z"
sed -i.bak -E 's/^version = "[0-9]+\.[0-9]+\.[0-9]+"/version = "'"$VERSION"'"/' "$CARGO" && rm "$CARGO.bak"

# Cargo.lock must follow, or the build fails on a version mismatch. This is not
# optional: forgetting it is how you discover the problem at compile time.
if command -v cargo &>/dev/null; then
    (cd "$RUST_DIR" && cargo update -p tune-server >/dev/null 2>&1) \
        && echo "  Cargo.lock updated" \
        || echo "  ! cargo update -p tune-server failed — run it by hand before committing"
fi

# 2. package.json — "version": "X.Y.Z"
sed -i.bak -E "s/\"version\": \"[0-9]+\\.[0-9]+\\.[0-9]+\"/\"version\": \"$VERSION\"/" "$WEB" && rm "$WEB.bak"

echo
echo "Bumped Tune to v$VERSION"
echo "  - $CARGO (+ Cargo.lock)"
echo "  - $WEB"

# 3 & 4. Clients — OPT-IN ONLY.
#
# Flutter and iPadOS run on their OWN release cadence, well behind the server:
# on 2026-08-08 the server was at 0.9.55 while Flutter sat at 0.9.42+477 and
# iPadOS at 0.9.28 — 13 and 27 versions apart. Their build numbers feed
# TestFlight, so bumping them as a side effect of a server release is wrong.
# Ask Bertrand before passing --clients.
if [ "$WITH_CLIENTS" -eq 1 ]; then
    # pubspec.yaml — version: X.Y.Z+N, build number strictly monotonic.
    CUR_BUILD=$(grep -oE "^version: [0-9]+\.[0-9]+\.[0-9]+\+[0-9]+" "$FLUTTER" | head -1 | sed -E 's/.*\+//')
    if [ -n "$CUR_BUILD" ]; then
        NEXT_FLUTTER=$((CUR_BUILD + 1))
        sed -i.bak -E "s/^version: [0-9]+\.[0-9]+\.[0-9]+\+[0-9]+/version: $VERSION+$NEXT_FLUTTER/" "$FLUTTER"
    else
        NEXT_FLUTTER=1
        sed -i.bak -E "s/^version: [0-9]+\.[0-9]+\.[0-9]+.*/version: $VERSION+1/" "$FLUTTER"
    fi
    rm "$FLUTTER.bak"

    # project.yml — MARKETING_VERSION everywhere; CURRENT_PROJECT_VERSION += 1
    # everywhere. Every Swift target must move together or TestFlight rejects
    # mismatched build numbers against the same bundle id.
    sed -i.bak -E "s/MARKETING_VERSION: \"[0-9]+\.[0-9]+\.[0-9]+\"/MARKETING_VERSION: \"$VERSION\"/g" "$IPAD"
    CURRENT_BUILD=$(grep -oE "CURRENT_PROJECT_VERSION: [0-9]+" "$IPAD" | head -1 | awk '{print $2}')
    NEXT_BUILD=$((CURRENT_BUILD + 1))
    sed -i.bak -E "s/CURRENT_PROJECT_VERSION: [0-9]+/CURRENT_PROJECT_VERSION: $NEXT_BUILD/g" "$IPAD"
    rm "$IPAD.bak"

    echo "  - $FLUTTER (build $NEXT_FLUTTER)"
    echo "  - $IPAD (build $NEXT_BUILD for Apple targets)"
else
    echo
    echo "  Clients NOT bumped (Flutter / iPadOS run their own cadence)."
    echo "  Pass --clients to include them — ask first."
fi

# NOTE — Homebrew is deliberately absent.
#
# The old version of this script rewrote homebrew/tune-server.rb locally and
# PUT it straight onto renesenses/homebrew-tap. That was actively harmful:
#   - it ran at bump time, when no release asset exists yet, so the sha256
#     values it pushed were whatever stale values the local copy carried;
#   - its filename regex only matched `vX.Y.Z.tar.gz`, never
#     `tune-server-vX.Y.Z-macos-aarch64.tar.gz`, so the local copy still
#     pointed at v0.8.90 assets that 404.
# Running it would have clobbered a working tap formula with a broken one.
#
# The tap is owned by the `homebrew` job in .github/workflows/release.yml,
# which runs AFTER the assets are published, downloads them, computes real
# SHA256s, and pushes as renesenses-bot. Leave it to CI.

echo
echo "Review with: git diff"
echo "Then: commit web on tune-web-client main and PUSH IT before tagging the server."
