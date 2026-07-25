#!/bin/sh
# Pre-tag guard: no merged-to-main PR may be missing from release/v0.9.
#
# The release tags are cut from release/v0.9; anything merged to main only
# NEVER reaches users (bitten repeatedly: #779 DSD perf absent from
# v0.9.0/v0.9.1, then a dozen PRs — #833 VU-meter included — absent from
# v0.9.8/9/10). git-cherry and content probes both proved unreliable; the
# only dependable check is PR-by-PR: is the merge commit an ancestor of
# release/v0.9, or does a port commit reference the PR number?
#
# Usage: scripts/preflight-port-audit.sh [days-back]   (default 7)
# Exit 1 if any PR is missing — the release pipeline must stop.
set -e
DAYS="${1:-7}"
SINCE=$(date -u -v-"${DAYS}"d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "-${DAYS} days" +%Y-%m-%dT%H:%M:%SZ)
git fetch -q origin release/v0.9 main
FLAG=$(mktemp)
rm -f "$FLAG"
gh pr list --state merged --base main --limit 100 \
  --json number,title,mergedAt,mergeCommit \
  -q ".[] | select(.mergedAt > \"$SINCE\") | (.number|tostring) + \"|\" + .mergeCommit.oid + \"|\" + .title" |
while IFS='|' read -r num sha title; do
  if git merge-base --is-ancestor "$sha" origin/release/v0.9 2>/dev/null; then
    continue
  fi
  # A port references the PR number in its subject — "(#833)", "#833:",
  # "(#833," … Match the number bounded by a non-digit so #72 never
  # matches #724.
  if git log origin/release/v0.9 --oneline --extended-regexp \
      --grep="#${num}([^0-9]|\$)" | grep -q .; then
    continue
  fi
  # Renumbered ports (original -> port PR). Extend when a port gets its
  # own PR number without referencing the original.
  case "$num" in
    761) continue ;; # porté via #762
    724) continue ;; # porté pré-v0.9.0 sans référence (8035693b)
  esac
  echo "MANQUANT sur release/v0.9 : PR #${num} — ${title}"
  touch "$FLAG"
done
if [ -e "$FLAG" ]; then
  rm -f "$FLAG"
  echo "⛔ Des PRs main-only ne sont pas portées — NE PAS TAGUER."
  exit 1
fi
echo "✅ Toutes les PRs mergées (${DAYS}j) sont sur release/v0.9."
