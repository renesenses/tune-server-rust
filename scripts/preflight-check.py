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

# Known people may use more than one clone or process, but these two addresses
# must never cross owners. Keep this deliberately narrow: the release gate is
# meant to reject the mixed pairs that have already forged attribution, not to
# turn every future contributor into an allow-list entry.
MIXED_IDENTITY_PAIRS = {
    ("bertrand", "jp@robbe.net"),
    ("renesenses", "jp@robbe.net"),
    ("jean-philippe robbe", "renesenses@gmail.com"),
    ("jprobbe", "renesenses@gmail.com"),
}


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


def check_identity_contract(
    identities: list[tuple[str, str, str]], name: str
) -> CheckResult:
    """Reject known name/email mixtures without imposing a contributor allow-list."""
    mixed = [
        f"{role}={person} <{email}>"
        for role, person, email in identities
        if (person.strip().casefold(), email.strip().casefold())
        in MIXED_IDENTITY_PAIRS
    ]
    rendered = "; ".join(
        f"{role}={person} <{email}>" for role, person, email in identities
    )
    if mixed:
        return CheckResult(
            name,
            False,
            "mixed Git identity: " + "; ".join(mixed),
        )
    return CheckResult(name, True, rendered)


def check_release_commit_identity() -> CheckResult:
    """Validate the author and committer stored on the commit being released."""
    revision = os.environ.get("GITHUB_SHA") or "HEAD"
    try:
        proc = subprocess.run(
            [
                "git",
                "show",
                "-s",
                "--format=%an%x00%ae%x00%cn%x00%ce",
                revision,
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return CheckResult("release_identity", False, f"cannot read Git identity: {exc}")
    fields = proc.stdout.rstrip("\n").split("\x00")
    if proc.returncode != 0 or len(fields) != 4:
        detail = proc.stderr.strip() or "unexpected git show output"
        return CheckResult("release_identity", False, detail)
    return check_identity_contract(
        [("author", fields[0], fields[1]), ("committer", fields[2], fields[3])],
        "release_identity",
    )


def _parse_git_var_identity(value: str) -> Optional[tuple[str, str]]:
    """Parse `git var GIT_*_IDENT` without depending on its timestamp."""
    match = re.match(r"^(.*) <([^<>]+)> \d+ [+-]\d{4}$", value.strip())
    if not match:
        return None
    return match.group(1), match.group(2)


def check_planned_git_identity() -> CheckResult:
    """Validate the identities Git would use for the next local commit."""
    identities: list[tuple[str, str, str]] = []
    for role, variable in [
        ("author", "GIT_AUTHOR_IDENT"),
        ("committer", "GIT_COMMITTER_IDENT"),
    ]:
        try:
            proc = subprocess.run(
                ["git", "var", variable],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            return CheckResult("planned_identity", False, f"cannot run git var: {exc}")
        parsed = _parse_git_var_identity(proc.stdout) if proc.returncode == 0 else None
        if parsed is None:
            detail = proc.stderr.strip() or f"cannot parse {variable}"
            return CheckResult("planned_identity", False, detail)
        identities.append((role, parsed[0], parsed[1]))
    return check_identity_contract(identities, "planned_identity")


def self_test_identity_contract() -> None:
    valid = check_identity_contract(
        [
            ("author", "Jean-Philippe ROBBE", "jp@robbe.net"),
            ("committer", "Bertrand", "renesenses@gmail.com"),
        ],
        "test",
    )
    assert valid.passed, valid

    forged_author = check_identity_contract(
        [("author", "Bertrand", "jp@robbe.net")], "test"
    )
    assert not forged_author.passed, forged_author
    assert "Bertrand <jp@robbe.net>" in forged_author.detail

    forged_committer = check_identity_contract(
        [("committer", "Jean-Philippe ROBBE", "renesenses@gmail.com")], "test"
    )
    assert not forged_committer.passed, forged_committer

    # Unknown contributors remain valid: this is a consistency guard, not an
    # allow-list that silently bars a new maintainer from making a release.
    unknown = check_identity_contract(
        [("author", "Nouvelle Mainteneuse", "maintainer@example.org")], "test"
    )
    assert unknown.passed, unknown

    assert _parse_git_var_identity("Bertrand <renesenses@gmail.com> 1 +0200") == (
        "Bertrand",
        "renesenses@gmail.com",
    )
    assert _parse_git_var_identity("invalid") is None


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


# ─── Portes de release ────────────────────────────────────────────────
#
# `ci_status` regardait TOUTE execution terminee du commit tague. Ce n'est pas
# un detail de tri : sur le commit d'une release, GitHub accroche aussi les
# sondes planifiees (`uptime-watch`, `forum-watch`), les robots d'issues
# (`fermeture-issues`), la machinerie du train (`release-controller`,
# `release` — donc le preflight LUI-MEME) et les AUDITS de gouvernance.
#
# Le 02/09, v0.9.131 a ete bloquee par trois « echecs » dont aucun n'etait un
# test : une tentative du controleur rejouee avec succes ensuite, la conclusion
# precedente du preflight lui-meme, et « Derive des garde-fous » — un audit
# declenche par le `push` de la promotion vers `main` (le commit touche
# `.github/workflows/**`), demarre 6 secondes apres le commit. Cet audit rougit
# PAR CONSTRUCTION pendant un train : il constate que le gel des tags est
# ouvert et que l'armement est a `true`, c'est-a-dire l'etat normal d'une
# release en cours. Les deux gardes se contredisent par definition.
#
# v0.9.130 avait le MEME audit en echec sur son commit ; elle est passee parce
# que l'audit n'y a tourne que 5 h apres son preflight (planification, pas
# `push` : sa promotion ne touchait pas de workflow). Ce n'etait donc pas une
# regression mais une course, gagnee par hasard de calendrier une fois et
# perdue la fois suivante.
#
# La regle retenue est une LISTE BLANCHE de WORKFLOWS, pas de noms de jobs.
# C'est le compromis qui evite le faux vert la ou le changement est routinier :
# ajouter un job a `ci.yml` (« Audio embedding feature », « Impact et
# garde-fous legers », une cible `Build …` de plus) arrive toutes les semaines
# et se retrouve compte AUTOMATIQUEMENT, alors qu'une liste blanche de noms
# l'aurait laisse tomber en silence. Seule la creation d'un nouveau FICHIER de
# workflow-porte demande de venir l'inscrire ici — un acte rare et delibere,
# visible en revue. Une liste noire d'audits connus a ete ecartee : ce depot
# ajoute des audits en permanence, et le prochain rebloquerait un train.
RELEASE_GATE_WORKFLOWS: tuple[str, ...] = (
    ".github/workflows/ci.yml",
    ".github/workflows/test-postgres.yml",
    ".github/workflows/widget-ci.yml",
)

# `ci.yml` est la seule des trois sans filtre `paths` sur `push: [main]` : elle
# tourne sur TOUT commit promu. Son absence n'est donc jamais « rien a
# signaler », c'est un garde qui ne trouve pas sa porte — et un garde qui ne
# trouve rien doit refuser. Les deux autres ont un filtre `paths` et peuvent
# legitimement manquer.
MANDATORY_GATE_WORKFLOW = ".github/workflows/ci.yml"


def release_gate_suites(repo: str, sha: str, token: Optional[str]) -> dict[int, str]:
    """Renvoie {check_suite_id: fichier de workflow} pour les seules portes."""
    suites: dict[int, str] = {}
    for workflow in RELEASE_GATE_WORKFLOWS:
        filename = workflow.rsplit("/", 1)[-1]
        data = github_api(
            f"/repos/{repo}/actions/workflows/{filename}/runs"
            f"?head_sha={sha}&per_page=100",
            token,
        )
        for wrun in data.get("workflow_runs", []):
            suite_id = wrun.get("check_suite_id")
            if suite_id is not None:
                suites[suite_id] = workflow
    return suites


def check_ci_status(repo: str, sha: str, token: Optional[str]) -> CheckResult:
    """Check that the release GATES on the tag commit are green.

    Les check-runs sont lus SUITE PAR SUITE (`/check-suites/{id}/check-runs`)
    et non via `/commits/{sha}/check-runs`. Ce dernier renvoie tout ce qui
    s'accroche au commit — 148 entrees sur le commit de la v0.9.130, donc
    au-dela de la premiere page : filtrer apres coup une liste tronquee ferait
    disparaitre les portes elles-memes. Interroger les suites des portes borne
    la lecture a ce qui compte.
    """
    try:
        gates = release_gate_suites(repo, sha, token)
        runs: list[dict] = []
        for suite_id in gates:
            data = github_api(
                f"/repos/{repo}/check-suites/{suite_id}/check-runs?per_page=100",
                token,
            )
            runs.extend(data.get("check_runs", []))
    except HTTPError as e:
        return CheckResult("ci_status", False, f"GitHub API error: {e.code}")
    except Exception as e:
        return CheckResult("ci_status", False, f"GitHub API error: {e}")

    if MANDATORY_GATE_WORKFLOW not in gates.values():
        return CheckResult(
            "ci_status",
            False,
            f"aucune execution de {MANDATORY_GATE_WORKFLOW} sur ce commit : "
            "porte de release introuvable",
        )
    if not runs:
        return CheckResult(
            "ci_status",
            False,
            "aucun check-run de porte de release sur ce commit",
        )

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
    gate_names = ", ".join(sorted({w.rsplit("/", 1)[-1] for w in gates.values()}))
    if pending:
        return CheckResult(
            "ci_status",
            True,
            f"portes de release vertes ({gate_names}) — "
            f"{len(pending)} encore en cours : {', '.join(pending[:5])}",
        )
    return CheckResult(
        "ci_status",
        True,
        f"{len(runs)} check-runs verts sur les portes de release ({gate_names})",
    )


def self_test_ci_status_gates() -> None:
    """Contre-epreuves du filtre : un audit ne bloque pas, une porte si."""

    def suite_runs(*names_states: tuple) -> dict:
        return {
            "check_runs": [
                {"name": name, "status": status, "conclusion": conclusion}
                for name, status, conclusion in names_states
            ]
        }

    # Suites reelles du commit b362f854 (v0.9.131). Les portes, la machinerie
    # du train et l'audit de gouvernance vivent dans des suites A PART.
    CI_SUITE = 91193662811
    PG_SUITE = 91193662862
    WIDGET_SUITE = 91193662822
    RELEASE_SUITE = 91208945815
    CONTROLLER_SUITE = 91207339518
    AUDIT_SUITE = 91300000001

    ci_gate_runs = suite_runs(
        ("Test", "completed", "success"),
        ("Clippy", "completed", "success"),
        ("Format", "completed", "success"),
        ("Build x86_64-unknown-linux-gnu", "completed", "success"),
        ("release-gate", "completed", "skipped"),
    )
    pg_gate_runs = suite_runs(("Test (PostgreSQL)", "completed", "success"))
    widget_gate_runs = suite_runs(("Widget (compilation)", "completed", "success"))

    # Le commit TEL QU'IL EST : les portes, plus tout ce qui s'y accroche sans
    # etre un test. La fausse API sert n'importe quel workflow demande — donc
    # elargir `RELEASE_GATE_WORKFLOWS` fait bel et bien rentrer le bruit, et
    # c'est ce qui rend le sabotage detectable.
    commit_suites = {
        "ci.yml": [CI_SUITE],
        "test-postgres.yml": [PG_SUITE],
        "widget-ci.yml": [WIDGET_SUITE],
        "release.yml": [RELEASE_SUITE],
        "release-controller.yml": [CONTROLLER_SUITE],
        "audit-derive.yml": [AUDIT_SUITE],
    }
    commit_bodies = {
        CI_SUITE: ci_gate_runs,
        PG_SUITE: pg_gate_runs,
        WIDGET_SUITE: widget_gate_runs,
        # La conclusion PRECEDENTE du preflight lui-meme.
        RELEASE_SUITE: suite_runs(
            ("Preflight / Pre-release checks", "completed", "failure"),
        ),
        # Un essai du controleur, bloque par un gel de tags, rejoue ensuite.
        CONTROLLER_SUITE: suite_runs(
            ("Verifier et taguer le train", "completed", "failure"),
        ),
        # L'audit de gouvernance, rouge PAR CONSTRUCTION pendant un train.
        AUDIT_SUITE: suite_runs(("Dérive des garde-fous", "completed", "failure")),
    }

    def make_api(workflow_suites: dict[str, list[int]], suite_bodies: dict[int, dict]):
        def fake(path: str, _token: Optional[str] = None):
            if "/actions/workflows/" in path:
                name = path.split("/actions/workflows/", 1)[1].split("/runs", 1)[0]
                return {
                    "workflow_runs": [
                        {"check_suite_id": sid}
                        for sid in workflow_suites.get(name, [])
                    ]
                }
            if "/check-suites/" in path:
                sid = int(path.split("/check-suites/", 1)[1].split("/", 1)[0])
                return suite_bodies[sid]
            # Notamment /commits/{sha}/check-runs : le tout-venant du commit
            # ne doit plus etre lu du tout.
            raise AssertionError(f"appel API inattendu: {path}")

        return fake

    original_github_api = globals()["github_api"]
    try:
        # Contrat 1 — LE BLOCAGE DU 02/09. Les vraies portes sont vertes ;
        # l'audit de gouvernance, la tentative du controleur et la conclusion
        # precedente du preflight sont rouges. Le preflight doit PASSER.
        globals()["github_api"] = make_api(commit_suites, commit_bodies)
        tonight = check_ci_status("owner/repo", "b362f854", None)
        assert tonight.passed, tonight
        assert "Dérive" not in tonight.detail, tonight
        assert "Verifier et taguer le train" not in tonight.detail, tonight
        assert "ci.yml" in tonight.detail, tonight

        # Contrat 2 — LA PORTE GARDE TOUJOURS. Une vraie porte rouge refuse,
        # sans quoi on aurait desarme le controle au lieu de l'affiner.
        broken_ci = dict(commit_bodies)
        broken_ci[CI_SUITE] = suite_runs(
            ("Test", "completed", "success"),
            ("Clippy", "completed", "failure"),
        )
        globals()["github_api"] = make_api(commit_suites, broken_ci)
        red = check_ci_status("owner/repo", "b362f854", None)
        assert not red.passed, red
        assert "Clippy" in red.detail, red

        # Une porte rouge dans une AUTRE suite-porte refuse aussi.
        broken_pg = dict(commit_bodies)
        broken_pg[PG_SUITE] = suite_runs(("Test (PostgreSQL)", "completed", "failure"))
        globals()["github_api"] = make_api(commit_suites, broken_pg)
        red_pg = check_ci_status("owner/repo", "b362f854", None)
        assert not red_pg.passed, red_pg
        assert "Test (PostgreSQL)" in red_pg.detail, red_pg

        # Contrat 3 — LE CAS VIDE. Aucune porte reconnue sur le commit :
        # c'est un REFUS, pas un « rien a signaler ». Le bruit reste present
        # et vert, et ne doit pas suffire a faire passer le controle.
        no_gates = dict(commit_suites)
        no_gates.update({"ci.yml": [], "test-postgres.yml": [], "widget-ci.yml": []})
        globals()["github_api"] = make_api(no_gates, commit_bodies)
        empty = check_ci_status("owner/repo", "b362f854", None)
        assert not empty.passed, empty
        assert MANDATORY_GATE_WORKFLOW in empty.detail, empty

        # Variante : une porte facultative a tourne, mais pas `ci.yml`, qui
        # n'a pas de filtre `paths` et tourne donc sur tout commit promu.
        # Refus egalement — la porte obligatoire manque.
        only_optional = dict(commit_suites)
        only_optional["ci.yml"] = []
        globals()["github_api"] = make_api(only_optional, commit_bodies)
        partial = check_ci_status("owner/repo", "b362f854", None)
        assert not partial.passed, partial
        assert MANDATORY_GATE_WORKFLOW in partial.detail, partial

        # Et une suite-porte VIDE de check-runs refuse aussi.
        hollow_bodies = dict(commit_bodies)
        hollow_bodies[CI_SUITE] = {"check_runs": []}
        globals()["github_api"] = make_api(
            {"ci.yml": [CI_SUITE], "test-postgres.yml": [], "widget-ci.yml": []},
            hollow_bodies,
        )
        hollow = check_ci_status("owner/repo", "b362f854", None)
        assert not hollow.passed, hollow

        # Contrat 4 — un job de porte encore en cours ne bloque pas.
        pending_bodies = dict(commit_bodies)
        pending_bodies[CI_SUITE] = suite_runs(
            ("Test", "completed", "success"),
            ("Build aarch64-apple-darwin", "in_progress", None),
        )
        globals()["github_api"] = make_api(commit_suites, pending_bodies)
        running = check_ci_status("owner/repo", "b362f854", None)
        assert running.passed, running
        assert "Build aarch64-apple-darwin" in running.detail, running
    finally:
        globals()["github_api"] = original_github_api


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
        "--identity-only",
        action="store_true",
        help="validate the author/committer Git would use for the next commit",
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
        self_test_identity_contract()
        self_test_ci_status_gates()
        print("preflight self-tests: PASS")
        return 0
    if args.identity_only:
        result = check_planned_git_identity()
        marker = "PASS" if result.passed else "FAIL"
        print(f"[{marker}] {result.name} — {result.detail}")
        return 0 if result.passed else 1
    if not args.version:
        ap.error("--version is required unless --self-test or --identity-only is used")

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
    run("release_identity", check_release_commit_identity)
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
