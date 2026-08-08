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
#   scripts/bump-all.sh 0.9.55 --skip-web-drift-check   # escape hatch, see below
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
SKIP_WEB_DRIFT=0
for arg in "$@"; do
    case "$arg" in
        --clients) WITH_CLIENTS=1 ;;
        --skip-web-drift-check) SKIP_WEB_DRIFT=1 ;;
        -*) echo "Error: unknown flag $arg" >&2; exit 2 ;;
        *) VERSION="$arg" ;;
    esac
done

if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version> [--clients] [--skip-web-drift-check]   (e.g. $0 0.9.55)" >&2
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

# Pre-flight: the web client must not be BEHIND its own development branch.
#
# release.yml builds the web client from tune-web-client `main` for a stable
# tag (`release/v0.9` is used only for `-rc` tags), but the day-to-day work
# lands on `release/v0.9`. When commits pile up there without reaching `main`,
# the release ships an OLD interface while the notes announce the new one.
#
# That is not hypothetical: 0.9.52 → 0.9.55 all shipped a stale bundle. Fifteen
# commits sat stranded — analog VU meters in TV mode, lyrics on radio/streaming
# tracks, smart-collection criteria, support-ticket attachments, the zone
# brand/model catalogue. Three of the four features announced for 0.9.54 had no
# interface at all, and a tester opened forum #1319 because the VU meters he
# had been promised did not exist in his binary. The drift then re-formed twice
# within a single day.
#
# So this aborts rather than warns: the whole point is that nobody notices
# until a tester does. Use --skip-web-drift-check only when you have decided,
# deliberately, that the stranded commits must not ship.
if [ "$SKIP_WEB_DRIFT" -eq 0 ] && [ -d "$DEV/tune-web-client/.git" ] && command -v git &>/dev/null; then
    WEB_REPO="$DEV/tune-web-client"
    if git -C "$WEB_REPO" fetch --quiet origin 2>/dev/null; then
        DRIFT=$(git -C "$WEB_REPO" rev-list --count origin/main..origin/release/v0.9 2>/dev/null || true)
        if [ -n "${DRIFT:-}" ] && [ "$DRIFT" -gt 0 ]; then
            echo >&2
            echo "Error: $DRIFT commit(s) live on tune-web-client origin/release/v0.9 but not on origin/main." >&2
            echo "       A stable tag builds the web client from main, so they would NOT ship." >&2
            echo >&2
            git -C "$WEB_REPO" log --oneline origin/main..origin/release/v0.9 | sed 's/^/         /' >&2
            echo >&2
            echo "       Fix: merge release/v0.9 into main on tune-web-client (a merge commit," >&2
            echo "            not a squash, so the trees converge), then re-run this bump." >&2
            echo "       Override: --skip-web-drift-check" >&2
            echo >&2
            exit 1
        fi
        echo "  Web drift check: main is in sync with release/v0.9"
    else
        echo "  ! could not fetch tune-web-client — web drift NOT verified" >&2
    fi
fi

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
# Their build numbers feed TestFlight and Firebase, where a number is spent for
# good: bumping them as a side effect of a server release is not something to
# decide on the author's behalf. Ask Bertrand before passing --clients.
#
# Opt-in, NOT "they lag far behind" — they track the server closely. Verified
# 2026-08-08: server 0.9.57, both clients bumped 0.9.56 → 0.9.57 in step.
#
# ⚠️ An earlier revision of this comment claimed Flutter sat at 0.9.42 and
# iPadOS at 0.9.28, "13 and 27 versions apart". That was wrong, and worth
# recording because the mistake is easy to repeat: those numbers were read from
# the LOCAL working copies of the client repos, which were parked on old
# branches. Always read a version from the branch that ships:
#
#   git -C "$DEV/tune-server-flutter" show origin/main:pubspec.yaml
#   git -C "$DEV/tune-server-ipados"  show origin/main:Tune/project.yml
#
# The same trap applies to tune-web-client's package.json.
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
    echo "  Clients NOT bumped (Flutter / iPadOS are opt-in)."
    echo "  Pass --clients to include them — ask first, their build numbers"
    echo "  are spent for good on TestFlight / Firebase."
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
