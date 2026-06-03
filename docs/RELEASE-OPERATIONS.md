# Release operations — runbook

How to ship a release with the autonomy tooling shipped in v0.8.28.
Every step is meant to be runnable by a single maintainer without
coordination, and to be reversible if something goes wrong.

References:
- Plan : [RELEASE-AUTONOMY-v0.9.50.md](RELEASE-AUTONOMY-v0.9.50.md)
- Workflows : `.github/workflows/{preflight,changelog,release,docker,rollback}.yml`
- CLI : `tune release …`

## TL;DR

```bash
# 1. Bump version
tune release bump patch --apply        # or minor / major

# 2. Commit and tag
git add Cargo.toml Cargo.lock
git commit -m "bump v0.8.NN"
git tag v0.8.NN
git push origin main --tags

# 3. Watch all five workflows go green
gh run list --limit 6

# 4. If something breaks, roll back via the dispatch workflow
gh workflow run rollback.yml \
  --field version=v0.8.NN \
  --field previous_version=v0.8.27 \
  --field apply=true
```

Everything else is described below.

## 1. Pre-flight checks

The `Preflight` workflow runs automatically on tag push (and via
manual dispatch). It blocks if any of the following fails:

- Tag is not valid semver (`vMAJOR.MINOR.PATCH[-PRE]`)
- Tag is not strictly greater than the version in `Cargo.toml`
- Any GitHub issue with label `P0` is open
- Any `TODO(release)` marker is present in source code (docs/ excluded)
- No `cahier-recette-v{major}.{minor}*.md` exists under `docs/`
- `cargo audit` reports a CVE in the dependency tree
- `cargo deny check` reports a license or duplicate-dep issue
- The CI status for the commit is not green

The workflow surfaces a failed status check on the commit; the release
pipeline does NOT automatically halt — that decision is yours. If the
preflight is red the right move is almost always to fix the underlying
issue first.

To run a manual dry-run before tagging:

```bash
gh workflow run preflight.yml \
  --field version=v0.8.28 \
  --field skip_ci_check=true
```

Or locally:

```bash
python3 scripts/preflight-check.py --version v0.8.28 --no-ci-check
```

## 2. Bump

`tune release bump <patch|minor|major>` rewrites `Cargo.toml` and
regenerates `Cargo.lock`. It runs in dry-run mode by default; pass
`--apply` to actually mutate the tree.

```bash
$ tune release bump patch
  Current : 0.8.27
  Bump    : patch
  Next    : 0.8.28

Dry run. Pass --apply to actually rewrite Cargo.toml + Cargo.lock.
```

The workspace `version` is the only authoritative value; sub-crates
inherit via `version.workspace = true`. If you want to audit other
files that might carry the version (NSIS installer, Tauri config,
Homebrew formula in another repo), run:

```bash
bash scripts/find-version-strings.sh
```

## 3. Tag and push

```bash
git add Cargo.toml Cargo.lock
git commit -m "bump v0.8.NN"
git tag v0.8.NN
git push origin main --tags
```

Six workflows kick off on the tag push:

| Workflow | Purpose |
|---|---|
| `Preflight` | Status check (blocks nothing automatically, see above) |
| `Release` | Builds 5 platform binaries (Linux x64/arm64, macOS x64/arm64, Windows x64), creates the GitHub Release, then runs the Homebrew + Forum jobs |
| `Docker` | Builds multi-arch image (linux/amd64 + linux/arm64) and pushes to `renesenses/tune:vN.N.N` + `:latest` on Docker Hub |
| `Changelog` | Generates `RELEASE_NOTES.md` via git-cliff, opens a PR back to main that refreshes `CHANGELOG.md` |
| `Tests (PostgreSQL)` | Runs the engine + postgres-skeleton tests against a live `postgres:16-alpine` service container |
| `CI` | The usual Format / Clippy / Test matrix |

## 4. Homebrew tap auto-update

The `homebrew` job in `release.yml` runs after `release`. It:

1. Fetches the source tarball from `https://github.com/renesenses/tune-server-rust/archive/refs/tags/vN.N.N.tar.gz`
2. Computes SHA256
3. Clones `renesenses/homebrew-tap` via the `HOMEBREW_TAP_TOKEN` secret
4. Rewrites `Formula/tune-server.rb` (`url`, `sha256`, `version` lines)
5. Commits `tune-server vN.N.N` and pushes

Users see the new version on `brew upgrade tune-server` (after at most
a `brew update`). Re-tap is not required.

## 5. Forum announcement

The `forum` job in `release.yml` runs after `release`. It:

1. Builds release notes with `git-cliff --tag vN.N.N --latest`
2. Posts a new **pinned** thread to `mozaiklabs.fr/api/v1/forum/threads`
   titled `[Release] tune-server vN.N.N disponible`
3. Body includes the changelog section + download links + Docker pull
   instructions + Homebrew install command

The thread is created in the `releases` category by default.

## 6. Rollback

When a release turns out broken after publication, the `Rollback`
workflow walks back the five publication surfaces. It is opt-in and
defaults to dry-run.

```bash
gh workflow run rollback.yml \
  --field version=v0.8.28 \
  --field previous_version=v0.8.27 \
  --field apply=false                  # dry-run first
```

A dry-run prints what would happen at each step. Re-run with
`apply=true` to actually mutate state.

Optional inputs:

- `delete_tag=true` — also force-deletes the git tag on the remote.
  Default: keep the tag for traceability.
- `forum_thread_id=12345` — post an erratum reply on the original
  announcement thread. Leave blank to skip.

What gets reverted:

| Surface | Action |
|---|---|
| GitHub Release | Marked as draft (assets stay reachable) |
| Git tag | Optional `git push --delete` |
| Homebrew tap | `git revert` of the bump commit |
| Docker Hub | Drop the bad tag + re-point `:latest` via `buildx imagetools` |
| Forum | Erratum reply posted on the original thread |

## 7. Required secrets

Configured on `renesenses/tune-server-rust`:

| Secret | Used by | Notes |
|---|---|---|
| `GITHUB_TOKEN` | preflight, release, changelog, rollback | Provided automatically by Actions |
| `DOCKERHUB_USERNAME` | docker, rollback | docker.io login |
| `DOCKERHUB_TOKEN` | docker, rollback | docker.io PAT |
| `HOMEBREW_TAP_TOKEN` | release.homebrew, rollback | Write access to `renesenses/homebrew-tap` |
| `FORUM_TOKEN` | release.forum, forum-watch, rollback | Bearer for mozaiklabs forum API |
| Apple signing | release | DMG signing (see existing secrets `APPLE_*`) |

## 8. Conventional commits

Phase 3 (auto-changelog) relies on commit messages following the
[conventional commits](https://www.conventionalcommits.org/) shape:

- `feat:` — new feature
- `fix:` — bug fix
- `perf:` — performance improvement
- `docs:` — documentation
- `refactor:` — code restructuring
- `test:` — test changes
- `ci:` — CI/build
- `chore:` — chores (boring stuff)
- `style:` — formatting

Commits that don't match end up in the "Other" group. Bump commits
matching `^bump v[0-9]` and `^Merge branch` lines are skipped from the
changelog by design.

## 9. Manual override paths

If you really need to bypass automation:

- **Skip preflight**: tag push triggers preflight but does not block
  release — just ignore the red status check.
- **Skip Homebrew**: comment out `secrets.HOMEBREW_TAP_TOKEN` in the
  job's `if`, or simply don't have the secret set.
- **Skip Forum**: same pattern with `FORUM_TOKEN`.
- **Skip Docker**: the `Docker` workflow is independent; just don't
  push the tag, or push to a non-`v*` ref.

The rollback workflow is also gated on `apply=true` so the only way
to accidentally mutate state is to explicitly opt in.

---

*Document évolutif. Dernière mise à jour : 2026-06-03.*
