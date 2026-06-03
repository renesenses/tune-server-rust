#!/usr/bin/env bash
# find-version-strings.sh
#
# Lists every place in the repo where the workspace version is mentioned,
# so a release bump can be audited before committing. Phase 2 of release
# autonomy (docs/RELEASE-AUTONOMY-v0.9.50.md).
#
# Use it before/after `tune release bump` to confirm no file was missed
# or contaminated with a stale version.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

WORKSPACE_VERSION="$(
    awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml
)"

if [ -z "$WORKSPACE_VERSION" ]; then
    echo "could not read workspace version from Cargo.toml" >&2
    exit 1
fi

echo "Workspace version: $WORKSPACE_VERSION"
echo

# Authoritative source.
echo "── Authoritative ──"
grep -nE '^version *=' Cargo.toml
echo

# Sub-crates: must inherit (workspace = true).
echo "── Sub-crate inheritance (should all be workspace=true) ──"
for cargo in tune-*/Cargo.toml; do
    line="$(grep -nE '^version' "$cargo" | head -1)"
    echo "  $cargo : $line"
done
echo

# Anywhere else in the repo that mentions the version literally.
echo "── Other literal mentions (informational, do not auto-bump) ──"
git grep -nE "$(echo "$WORKSPACE_VERSION" | sed 's/\./\\./g')" \
    -- ':(exclude)Cargo.toml' ':(exclude)Cargo.lock' \
    ':(exclude)target/' ':(exclude)docs/' \
    ':(exclude)scripts/find-version-strings.sh' \
    2>/dev/null || echo "  (none)"
echo

# Files that look like they might carry version (.nsi / .plist / .rb / Tauri),
# without filtering on the current version — useful right after bump to spot
# missed updates.
echo "── Files that typically carry version (review manually if you bumped) ──"
find . \
    -path ./target -prune -o \
    -path ./node_modules -prune -o \
    \( -name '*.nsi' -o -name 'project.yml' \
       -o -name 'tauri.conf*' -o -name '*.plist' \
       -o -name 'package.json' -o -name '*.rb' \
       -o -name 'Dockerfile*' \) \
    -print 2>/dev/null | grep -v '^./target' | head -20 || echo "  (none in this repo)"
echo

echo "Done."
