#!/usr/bin/env python3
"""
Preflight checks for Tune releases (phase 1 of release autonomy).

Run locally:
    python3 scripts/preflight-check.py --version v0.8.30

Run in CI:
    GITHUB_REPOSITORY=renesenses/tune-server-rust \
    GITHUB_TOKEN=$GITHUB_TOKEN \
    python3 scripts/preflight-check.py --version $GITHUB_REF_NAME

Exit code 0 = all checks pass, 1 = at least one failure.

Each check prints `[PASS]` or `[FAIL]` with a one-line reason. The
workflow surfaces the failures as a red status check, blocking the
release pipeline.

Skippable checks (degrade gracefully when their tool is missing):
- cargo audit, cargo deny — run on best effort, warn if absent.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional
from urllib.error import HTTPError
from urllib.request import Request, urlopen

REPO_ROOT = Path(__file__).resolve().parent.parent
SEMVER_RE = re.compile(
    r"^v(?P<major>\d+)\.(?P<minor>\d+)\.(?P<patch>\d+)(?:-(?P<pre>[0-9A-Za-z.-]+))?$"
)


@dataclass
class CheckResult:
    name: str
    passed: bool
    detail: str


def parse_semver(tag: str) -> Optional[tuple]:
    """Return (major, minor, patch, prerelease|None) or None."""
    m = SEMVER_RE.match(tag)
    if not m:
        return None
    return (
        int(m.group("major")),
        int(m.group("minor")),
        int(m.group("patch")),
        m.group("pre"),
    )


def read_workspace_version() -> Optional[str]:
    cargo_toml = REPO_ROOT / "Cargo.toml"
    if not cargo_toml.exists():
        return None
    for line in cargo_toml.read_text().splitlines():
        m = re.match(r'^version\s*=\s*"([^"]+)"', line.strip())
        if m:
            return m.group(1)
    return None


def github_api(path: str, token: Optional[str] = None) -> dict | list:
    """Call GitHub API with optional auth token."""
    url = f"https://api.github.com{path}"
    headers = {"Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = Request(url, headers=headers)
    with urlopen(req, timeout=30) as resp:
        return json.load(resp)


# ─── Individual checks ────────────────────────────────────────────────


def check_semver(tag: str) -> CheckResult:
    parsed = parse_semver(tag)
    if parsed is None:
        return CheckResult("semver", False, f"tag '{tag}' is not a valid semver (vX.Y.Z[-PRE])")
    return CheckResult("semver", True, f"{tag} is valid semver")


def check_version_bump(tag: str) -> CheckResult:
    parsed = parse_semver(tag)
    if parsed is None:
        return CheckResult("version_bump", False, "tag is not semver, skipping comparison")
    current = read_workspace_version()
    if current is None:
        return CheckResult("version_bump", False, "could not read Cargo.toml version")
    cur_parsed = parse_semver(f"v{current}")
    if cur_parsed is None:
        return CheckResult("version_bump", False, f"Cargo.toml version '{current}' is not semver")
    tag_tuple = parsed[:3]
    cur_tuple = cur_parsed[:3]
    if tag_tuple <= cur_tuple:
        return CheckResult(
            "version_bump",
            False,
            f"tag {tag_tuple} must be greater than Cargo.toml version {cur_tuple}",
        )
    return CheckResult(
        "version_bump",
        True,
        f"tag {tag_tuple} > Cargo.toml {cur_tuple}",
    )


def check_no_p0_issues(repo: str, token: Optional[str]) -> CheckResult:
    try:
        issues = github_api(
            f"/repos/{repo}/issues?state=open&labels=P0&per_page=100",
            token,
        )
    except HTTPError as e:
        return CheckResult("no_p0_issues", False, f"GitHub API error: {e.code}")
    except Exception as e:
        return CheckResult("no_p0_issues", False, f"GitHub API error: {e}")
    # Epics are tracking umbrellas, not single actionable blockers — an open
    # P0 epic shouldn't gate every release. Its child P0 issues are counted
    # individually and still block.
    def is_epic(issue: dict) -> bool:
        return any(lbl.get("name") == "epic" for lbl in issue.get("labels", []))

    open_p0 = [i for i in issues if "pull_request" not in i and not is_epic(i)]
    if open_p0:
        nums = ", ".join(f"#{i['number']}" for i in open_p0[:10])
        return CheckResult(
            "no_p0_issues",
            False,
            f"{len(open_p0)} P0 issues open: {nums}",
        )
    return CheckResult("no_p0_issues", True, "0 open P0 issues (epics excluded)")


def check_no_release_todos() -> CheckResult:
    """Grep for TODO(release) markers in source code.

    The docs/ tree, this script, and .github/ are excluded — those mentions
    describe the marker convention itself (e.g. the preflight.yml comment
    documenting this very check) and shouldn't block a release.
    """
    try:
        proc = subprocess.run(
            [
                "git",
                "grep",
                "-n",
                "-E",
                "TODO\\(release\\)",
                "--",
                ":(exclude)docs/",
                ":(exclude)scripts/preflight-check.py",
                ":(exclude).github/",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        return CheckResult("no_release_todos", False, "git not installed")
    if proc.returncode == 0:
        lines = proc.stdout.strip().splitlines()
        first = lines[0] if lines else "(unknown)"
        return CheckResult(
            "no_release_todos",
            False,
            f"{len(lines)} TODO(release) found, first: {first[:120]}",
        )
    return CheckResult("no_release_todos", True, "no TODO(release) markers in code")


def check_cahier_de_recette(tag: str) -> CheckResult:
    parsed = parse_semver(tag)
    if parsed is None:
        return CheckResult("cahier_de_recette", False, "tag not semver, cannot infer doc path")
    major, minor, patch, _ = parsed
    candidates = [
        REPO_ROOT / "docs" / f"cahier-recette-v{major}.{minor}.{patch}.md",
        REPO_ROOT / "docs" / f"cahier-recette-v{major}.{minor}.md",
        # Allow the cahier of the latest minor we have to satisfy patches.
        REPO_ROOT / "docs" / f"cahier-recette-v{major}.{minor - 1}.md" if minor > 0 else None,
    ]
    for c in candidates:
        if c is None:
            continue
        if c.exists():
            return CheckResult(
                "cahier_de_recette",
                True,
                f"found {c.relative_to(REPO_ROOT)}",
            )
    # Fallback: glob for any cahier-recette-v{major}.{minor}*.md
    docs = REPO_ROOT / "docs"
    pattern = f"cahier-recette-v{major}.{minor}*.md"
    matches = list(docs.glob(pattern))
    if matches:
        return CheckResult(
            "cahier_de_recette",
            True,
            f"found {matches[0].relative_to(REPO_ROOT)}",
        )
    return CheckResult(
        "cahier_de_recette",
        False,
        f"no cahier-recette-v{major}.{minor}*.md in docs/",
    )


def check_cargo_audit() -> CheckResult:
    if shutil.which("cargo-audit") is None:
        return CheckResult("cargo_audit", True, "cargo-audit not installed, skipping (warn)")
    try:
        proc = subprocess.run(
            ["cargo", "audit", "--quiet"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=300,
        )
    except subprocess.TimeoutExpired:
        return CheckResult("cargo_audit", False, "cargo audit timed out after 5min")
    except FileNotFoundError:
        return CheckResult("cargo_audit", True, "cargo binary missing, skipping")
    if proc.returncode != 0:
        first_line = (proc.stderr or proc.stdout).strip().splitlines()
        snippet = first_line[0] if first_line else "(no output)"
        return CheckResult(
            "cargo_audit",
            False,
            f"cargo audit failed: {snippet[:200]}",
        )
    return CheckResult("cargo_audit", True, "no known CVEs")


def check_cargo_deny() -> CheckResult:
    if shutil.which("cargo-deny") is None:
        return CheckResult("cargo_deny", True, "cargo-deny not installed, skipping (warn)")
    try:
        proc = subprocess.run(
            ["cargo", "deny", "check"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=300,
        )
    except subprocess.TimeoutExpired:
        return CheckResult("cargo_deny", False, "cargo deny timed out after 5min")
    except FileNotFoundError:
        return CheckResult("cargo_deny", True, "cargo binary missing, skipping")
    if proc.returncode != 0:
        # Surface the actual advisory/license/ban so the failure is diagnosable
        # (previously swallowed — a red cargo_deny gave no clue what broke).
        detail = (proc.stderr or proc.stdout or "").strip()
        deny_lines = [
            ln
            for ln in detail.splitlines()
            if any(k in ln for k in ("error[", "warning[", "RUSTSEC", "= note", "denied"))
        ]
        snippet = " | ".join(deny_lines[:5]) or detail[-300:]
        return CheckResult(
            "cargo_deny",
            False,
            f"cargo deny check failed (exit {proc.returncode}): {snippet}",
        )
    return CheckResult("cargo_deny", True, "licenses + duplicates clean")


def check_ci_status(repo: str, sha: str, token: Optional[str]) -> CheckResult:
    """Check that all completed CI check-runs on the tag commit are success."""
    try:
        data = github_api(
            f"/repos/{repo}/commits/{sha}/check-runs?per_page=100",
            token,
        )
    except HTTPError as e:
        return CheckResult("ci_status", False, f"GitHub API error: {e.code}")
    except Exception as e:
        return CheckResult("ci_status", False, f"GitHub API error: {e}")
    runs = data.get("check_runs", [])
    if not runs:
        return CheckResult("ci_status", False, "no CI check-runs found on this commit")
    failures = [
        r["name"]
        for r in runs
        if r.get("status") == "completed" and r.get("conclusion") not in ("success", "neutral", "skipped")
    ]
    if failures:
        return CheckResult(
            "ci_status",
            False,
            f"{len(failures)} failed: {', '.join(failures[:5])}",
        )
    pending = [r["name"] for r in runs if r.get("status") != "completed"]
    if pending:
        return CheckResult(
            "ci_status",
            False,
            f"{len(pending)} still in progress: {', '.join(pending[:5])}",
        )
    return CheckResult(
        "ci_status",
        True,
        f"all {len(runs)} check-runs green",
    )


# ─── Main ─────────────────────────────────────────────────────────────


def get_commit_sha() -> Optional[str]:
    """Return the current commit SHA (works in CI and locally)."""
    sha = os.environ.get("GITHUB_SHA")
    if sha:
        return sha
    try:
        proc = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if proc.returncode == 0:
            return proc.stdout.strip()
    except Exception:
        pass
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--version", required=True, help="release tag, e.g. v0.8.30")
    ap.add_argument(
        "--skip",
        default="",
        help="comma-separated check names to skip (advanced, use sparingly)",
    )
    ap.add_argument(
        "--no-ci-check",
        action="store_true",
        help="skip the GitHub CI status check (useful for local dry-runs)",
    )
    args = ap.parse_args()

    tag = args.version
    if not tag.startswith("v"):
        tag = f"v{tag}"

    repo = os.environ.get("GITHUB_REPOSITORY", "renesenses/tune-server-rust")
    token = os.environ.get("GITHUB_TOKEN")
    sha = get_commit_sha()
    skips = {s.strip() for s in args.skip.split(",") if s.strip()}

    checks: list[CheckResult] = []

    def run(name: str, fn):
        if name in skips:
            checks.append(CheckResult(name, True, "skipped by --skip"))
            return
        checks.append(fn())

    run("semver", lambda: check_semver(tag))
    run("version_bump", lambda: check_version_bump(tag))
    run("no_p0_issues", lambda: check_no_p0_issues(repo, token))
    run("no_release_todos", check_no_release_todos)
    run("cahier_de_recette", lambda: check_cahier_de_recette(tag))
    run("cargo_audit", check_cargo_audit)
    run("cargo_deny", check_cargo_deny)
    if not args.no_ci_check and sha:
        run("ci_status", lambda: check_ci_status(repo, sha, token))
    elif not args.no_ci_check:
        checks.append(CheckResult("ci_status", False, "no commit SHA available"))

    # Print summary
    print()
    print(f"Preflight checks for {tag} on {repo}@{sha[:10] if sha else '?'}")
    print("─" * 70)
    for c in checks:
        marker = "[PASS]" if c.passed else "[FAIL]"
        print(f"  {marker}  {c.name:25s}  {c.detail}")
    print("─" * 70)
    failed = [c for c in checks if not c.passed]
    if failed:
        print(f"  → {len(failed)} check(s) failed: " + ", ".join(c.name for c in failed))
        return 1
    print(f"  → all {len(checks)} checks passed, release can proceed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
