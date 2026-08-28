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
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional
from urllib.error import HTTPError
from urllib.request import Request, urlopen

REPO_ROOT = Path(__file__).resolve().parent.parent
SEMVER_RE = re.compile(
    r"^v(?P<major>\d+)\.(?P<minor>\d+)\.(?P<patch>\d+)(?:-(?P<pre>[0-9A-Za-z.-]+))?$"
)
P0_VERIFICATION_PENDING_LABEL = "release:verification-pending"
# Exemption « terrain » : une P0 dont le traitement est suspendu faute de
# reproduction, de materiel ou d'informations du testeur ne peut pas etre
# fermee par nous — la garder bloquante gelerait la ligne de release pour une
# duree qui ne depend pas de l'equipe.
#
# Elle ne bloque donc pas, MAIS elle doit se voir. Une exemption silencieuse
# recreerait exactement le defaut qu'on vient de supprimer : cinq releases
# publiees sur un preflight rouge sans que personne le remarque. Chaque P0
# exemptee est donc annoncee, avec son numero, dans le RESUME du job — pas
# seulement dans le journal, que personne ne deroule.
P0_FIELD_BLOCKED_LABEL = "bloque:terrain"


@dataclass
class CheckResult:
    name: str
    passed: bool
    detail: str
    # Avertissements remontes au resume du job (voir emit_job_summary).
    warnings: list[str] = field(default_factory=list)


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
    # The standard flow (bump-all.sh) bumps Cargo.toml to the release version and
    # tags the same version → tag == Cargo is the NORMAL case and must pass. Only
    # fail if the tag is strictly BEHIND Cargo.toml (you forgot to bump the tag).
    if tag_tuple < cur_tuple:
        return CheckResult(
            "version_bump",
            False,
            f"tag {tag_tuple} is behind Cargo.toml version {cur_tuple} (bump the tag)",
        )
    return CheckResult(
        "version_bump",
        True,
        f"tag {tag_tuple} >= Cargo.toml {cur_tuple}",
    )


def classify_open_p0_issues(
    issues: list[dict],
) -> tuple[list[dict], list[dict], list[dict]]:
    """Split actionable P0 issues into blockers, fixes to verify, exemptions.

    `keep-open`, `en-cours`, assignment and work locks deliberately have no
    special meaning here. Two labels, and two only, take a P0 off the blocking
    list, and epics remain excluded as before:

    - `release:verification-pending` : le correctif est fusionne, l'issue
      attend la release pour etre verifiee. Sans elle, la release attend la
      fermeture et la fermeture attend la release.
    - `bloque:terrain` : le traitement est suspendu faute de reproduction, de
      materiel ou d'informations du terrain.

    Les deux listes sont calculees INDEPENDAMMENT l'une de l'autre : une P0
    qui porte les deux etiquettes apparait dans les deux, donc deux fois dans
    le resume. C'est voulu — une exemption doit etre bruyante, pas discrete.
    """

    def labels(issue: dict) -> set[str]:
        return {
            label.get("name", "")
            for label in issue.get("labels", [])
            if isinstance(label, dict)
        }

    actionable = [
        issue
        for issue in issues
        if "pull_request" not in issue and "epic" not in labels(issue)
    ]
    awaiting_verification = [
        issue
        for issue in actionable
        if P0_VERIFICATION_PENDING_LABEL in labels(issue)
    ]
    field_blocked = [
        issue for issue in actionable if P0_FIELD_BLOCKED_LABEL in labels(issue)
    ]
    blocking = [
        issue
        for issue in actionable
        if P0_VERIFICATION_PENDING_LABEL not in labels(issue)
        and P0_FIELD_BLOCKED_LABEL not in labels(issue)
    ]
    return blocking, awaiting_verification, field_blocked


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
    blocking, awaiting_verification, field_blocked = classify_open_p0_issues(issues)
    pending_nums = ", ".join(
        f"#{issue['number']}" for issue in awaiting_verification[:10]
    )
    # Un avertissement PAR issue exemptee, avec son numero et son titre : c'est
    # ce que le resume du job affichera, et c'est la seule trace qu'une P0 est
    # sortie du chemin bloquant.
    warnings = [
        f"P0 #{issue['number']} ne bloque PAS la release — exemptee par "
        f"`{P0_FIELD_BLOCKED_LABEL}` : "
        f"{str(issue.get('title') or '').strip()[:120]}"
        for issue in field_blocked
    ]
    if blocking:
        nums = ", ".join(f"#{issue['number']}" for issue in blocking[:10])
        pending_detail = (
            f"; {len(awaiting_verification)} awaiting release verification: "
            f"{pending_nums}"
            if awaiting_verification
            else ""
        )
        exempt_detail = (
            f"; {len(field_blocked)} exemptees par {P0_FIELD_BLOCKED_LABEL}"
            if field_blocked
            else ""
        )
        return CheckResult(
            "no_p0_issues",
            False,
            f"{len(blocking)} blocking P0 issues open: {nums}"
            f"{pending_detail}{exempt_detail}",
            warnings,
        )
    if awaiting_verification or field_blocked:
        parts = ["0 blocking P0 issues"]
        if awaiting_verification:
            parts.append(
                f"{len(awaiting_verification)} awaiting release verification: "
                f"{pending_nums}"
            )
        if field_blocked:
            exempt_nums = ", ".join(
                f"#{issue['number']}" for issue in field_blocked[:10]
            )
            parts.append(
                f"{len(field_blocked)} exemptees par "
                f"{P0_FIELD_BLOCKED_LABEL}: {exempt_nums}"
            )
        return CheckResult("no_p0_issues", True, "; ".join(parts), warnings)
    return CheckResult("no_p0_issues", True, "0 blocking P0 issues (epics excluded)")


def self_test_p0_classification() -> None:
    """Counter-examples for every label that could be mistaken as an escape."""

    def issue(number: int, *label_names: str, pull_request: bool = False) -> dict:
        value = {
            "number": number,
            "labels": [{"name": name} for name in label_names],
        }
        if pull_request:
            value["pull_request"] = {"url": "https://example.invalid/pr"}
        return value

    examples = [
        issue(1, "P0"),
        issue(2, "P0", "keep-open"),
        issue(3, "P0", "en-cours", "verrou:issue-3"),
        issue(4, "P0", P0_VERIFICATION_PENDING_LABEL),
        issue(5, "P0", "epic"),
        issue(6, "P0", pull_request=True),
        issue(7, "P0", P0_FIELD_BLOCKED_LABEL),
        issue(8, "P0", P0_VERIFICATION_PENDING_LABEL, P0_FIELD_BLOCKED_LABEL),
        # Contre-exemple : une etiquette qui RESSEMBLE a l'exemption n'en est
        # pas une. Seul le nom exact sort une P0 du chemin bloquant.
        issue(9, "P0", "bloque:arbitrage"),
    ]
    blocking, awaiting, exempt = classify_open_p0_issues(examples)
    assert [value["number"] for value in blocking] == [1, 2, 3, 9], blocking
    assert [value["number"] for value in awaiting] == [4, 8], awaiting
    assert [value["number"] for value in exempt] == [7, 8], exempt

    original_github_api = globals()["github_api"]
    try:
        globals()["github_api"] = lambda _path, _token=None: examples
        mixed = check_no_p0_issues("owner/repo", None)
        assert not mixed.passed
        assert "4 blocking P0 issues open: #1, #2, #3, #9" in mixed.detail
        assert "2 awaiting release verification: #4, #8" in mixed.detail

        globals()["github_api"] = lambda _path, _token=None: [examples[3]]
        pending_only = check_no_p0_issues("owner/repo", None)
        assert pending_only.passed
        assert pending_only.detail.endswith("awaiting release verification: #4")
        assert pending_only.warnings == []

        # Contrat 1 — une P0 SANS exemption bloque toujours.
        globals()["github_api"] = lambda _path, _token=None: [examples[0]]
        bare = check_no_p0_issues("owner/repo", None)
        assert not bare.passed, bare
        assert "1 blocking P0 issues open: #1" in bare.detail

        # Contrat 2 — une P0 `bloque:terrain` ne bloque pas, et son NUMERO
        # apparait dans un avertissement destine au resume du job.
        globals()["github_api"] = lambda _path, _token=None: [examples[6]]
        exempted = check_no_p0_issues("owner/repo", None)
        assert exempted.passed, exempted
        assert f"exemptees par {P0_FIELD_BLOCKED_LABEL}: #7" in exempted.detail
        assert len(exempted.warnings) == 1, exempted.warnings
        assert "#7" in exempted.warnings[0]
        assert P0_FIELD_BLOCKED_LABEL in exempted.warnings[0]
        summary = render_job_summary("v9.9.9", "owner/repo", None, [exempted])
        assert "#7" in summary, summary
        assert "Avertissements" in summary, summary

        # Une exemption ne rachete PAS une autre P0 bloquante.
        globals()["github_api"] = lambda _path, _token=None: [
            examples[0],
            examples[6],
        ]
        mixed_exempt = check_no_p0_issues("owner/repo", None)
        assert not mixed_exempt.passed, mixed_exempt
        assert "1 blocking P0 issues open: #1" in mixed_exempt.detail
        assert f"1 exemptees par {P0_FIELD_BLOCKED_LABEL}" in mixed_exempt.detail
        assert "#7" in " ".join(mixed_exempt.warnings)
    finally:
        globals()["github_api"] = original_github_api


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
        lines = detail.splitlines()
        # Les ERREURS d'abord, et l'identifiant RUSTSEC avec elles.
        #
        # L'ancien filtre prenait les cinq premieres lignes correspondantes,
        # dans l'ordre du rapport. Or cargo-deny sort ses `warning[duplicate]`
        # AVANT ses `error[...]` : le message ne montrait donc que des doublons
        # de crates, parfaitement benins (`multiple-versions = "warn"`), et
        # jamais la cause reelle. Le preflight a echoue ainsi sur CHAQUE release
        # de la 0.9.58 a la 0.9.65 — huit versions — sans que le rapport permette
        # de savoir que le coupable etait un avis RUSTSEC sur audiopus_sys.
        #
        # Un controle rouge en permanence est un controle que plus personne ne
        # lit. C'etait le cas, et c'est ce message qui l'a rendu illisible.
        errors = [ln for ln in lines if "error[" in ln or "denied" in ln]
        ids = [ln.strip() for ln in lines if "RUSTSEC" in ln and "ID:" in ln]
        others = [ln for ln in lines if "warning[" in ln or "= note" in ln]
        deny_lines = errors + ids + others
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
    # Jobs that haven't finished yet are NOT a failure: the current preflight
    # check and other workflows triggered by the tag can still be running.
    # Only actually-failed runs (handled above) block. Release itself cannot
    # start building before this reusable workflow has succeeded.
    pending = [r["name"] for r in runs if r.get("status") != "completed"]
    if pending:
        return CheckResult(
            "ci_status",
            True,
            f"completed check-runs green ({len(pending)} still running: {', '.join(pending[:5])})",
        )
    return CheckResult(
        "ci_status",
        True,
        f"all {len(runs)} check-runs green",
    )


# ─── Resume du job ────────────────────────────────────────────────────


def render_job_summary(
    tag: str,
    repo: str,
    sha: Optional[str],
    checks: list[CheckResult],
) -> str:
    """Markdown du resume de job : etat de chaque controle + avertissements.

    Le journal d'un job ne se deroule pas : il faut ouvrir le run, ouvrir le
    job, ouvrir l'etape. Le resume, lui, s'affiche sur la page du run. Une
    exemption de P0 qui n'apparaitrait que dans le journal serait une
    exemption invisible — c'est-a-dire le defaut qu'on corrige.
    """
    lines = [
        f"## Preflight {tag}",
        "",
        f"`{repo}` @ `{sha[:10] if sha else '?'}`",
        "",
        "| | Controle | Detail |",
        "| :-: | --- | --- |",
    ]
    for check in checks:
        marker = "✅" if check.passed else "❌"
        detail = check.detail.replace("|", "\\|")
        lines.append(f"| {marker} | `{check.name}` | {detail} |")
    warnings = [w for check in checks for w in check.warnings]
    if warnings:
        lines += ["", "### ⚠️ Avertissements", ""]
        lines += [f"- ⚠️ {w.replace('|', chr(92) + '|')}" for w in warnings]
    failed = [c for c in checks if not c.passed]
    lines += [""]
    if failed:
        lines.append(
            f"**{len(failed)} controle(s) en echec : "
            + ", ".join(f"`{c.name}`" for c in failed)
            + "** — la release ne peut pas partir."
        )
    else:
        lines.append(f"**Les {len(checks)} controles passent.**")
    return "\n".join(lines) + "\n"


def emit_job_summary(
    tag: str,
    repo: str,
    sha: Optional[str],
    checks: list[CheckResult],
) -> None:
    """Ecrit le resume dans $GITHUB_STEP_SUMMARY et annote les avertissements."""
    for warning in (w for check in checks for w in check.warnings):
        print(f"::warning title=Preflight::{warning}")
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(render_job_summary(tag, repo, sha, checks))
    except OSError as e:
        # Ne jamais faire echouer le preflight sur l'ecriture du resume : le
        # verdict des controles prime.
        print(f"::warning::resume du job non ecrit ({e})")


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
    ap.add_argument("--version", help="release tag, e.g. v0.8.30")
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run local counter-examples without GitHub or cargo",
    )
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

    if args.self_test:
        self_test_p0_classification()
        print("preflight P0 classification self-test: PASS")
        return 0
    if not args.version:
        ap.error("--version is required unless --self-test is used")

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
        for w in c.warnings:
            print(f"  [WARN]  {c.name:25s}  {w}")
    print("─" * 70)
    emit_job_summary(tag, repo, sha, checks)
    failed = [c for c in checks if not c.passed]
    if failed:
        print(f"  → {len(failed)} check(s) failed: " + ", ".join(c.name for c in failed))
        return 1
    print(f"  → all {len(checks)} checks passed, release can proceed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
