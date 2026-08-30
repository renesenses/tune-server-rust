#!/usr/bin/env python3
"""Validate and report the canonical Tune release policy.

The validator intentionally uses only the Python standard library so the same
verdict can run in all four repositories and on managed agent runners.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any


POLICY_REL = Path(".github/release-policy.json")
SCHEMA_REL = Path(".github/release-policy.schema.json")
DIGEST_REL = Path(".github/release-policy.sha256")

TOP_LEVEL_KEYS = {
    "$schema",
    "schema_version",
    "policy_version",
    "mode",
    "repositories",
    "branch_classes",
    "checks",
    "ci_profiles",
    "roles",
    "transitions",
    "changes",
    "agents",
    "release",
    "ownership",
}
REPOSITORIES = {
    "server": "renesenses/tune-server-rust",
    "web": "renesenses/tune-web-client",
    "os": "renesenses/tune-os",
    "universal": "renesenses/tune-server-universal",
}
BRANCH_CLASSES = {"fix", "feat", "restore", "batch", "rc", "main"}
CHECKS = {
    "policy-gate",
    "issue-references",
    "format-static",
    "unit-targeted",
    "minimal-build",
    "release-gate",
    "sqlite-workspace",
    "postgresql",
    "shipped-features",
    "audio-embedding",
    "ffi-android",
    "windows-asio",
    "macos-runtime",
    "web-contract",
    "web-build",
    "security-audit",
    "preflight",
    "immutable-manifest",
    "tree-attestation",
    "os-first-boot",
}
ROLES = {"fix_agent", "integrator", "release_controller", "release_admin"}
ACTORS = {"openai", "anthropic", "human", "github_actions"}
TRANSITIONS = {"unit_to_integration", "integration_to_main"}
PROFILES = {"quick", "full"}


class PolicyError(ValueError):
    """A fail-closed policy validation error."""


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise PolicyError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _load_json(path: Path) -> dict[str, Any]:
    try:
        with path.open(encoding="utf-8") as handle:
            value = json.load(handle, object_pairs_hook=_reject_duplicate_keys)
    except (OSError, json.JSONDecodeError) as exc:
        raise PolicyError(f"cannot read {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise PolicyError(f"{path} must contain a JSON object")
    return value


def _exact_keys(value: dict[str, Any], expected: set[str], where: str) -> None:
    observed = set(value)
    if observed != expected:
        missing = sorted(expected - observed)
        unknown = sorted(observed - expected)
        raise PolicyError(f"{where}: missing={missing}, unknown={unknown}")


def _string_list(value: Any, where: str, *, nonempty: bool = False) -> list[str]:
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise PolicyError(f"{where} must be a list of strings")
    if nonempty and not value:
        raise PolicyError(f"{where} cannot be empty")
    if len(value) != len(set(value)):
        raise PolicyError(f"{where} contains duplicates")
    return value


def _known_refs(values: Any, known: set[str], where: str) -> list[str]:
    refs = _string_list(values, where, nonempty=True)
    unknown = sorted(set(refs) - known)
    if unknown:
        raise PolicyError(f"{where} references unknown values: {unknown}")
    return refs


def _validate_schema_file(root: Path) -> None:
    schema = _load_json(root / SCHEMA_REL)
    if schema.get("$schema") != "https://json-schema.org/draft/2020-12/schema":
        raise PolicyError("release-policy.schema.json uses an unknown schema draft")
    if schema.get("additionalProperties") is not False:
        raise PolicyError("release-policy.schema.json must reject unknown top-level fields")
    if set(schema.get("required", [])) != TOP_LEVEL_KEYS:
        raise PolicyError("release-policy.schema.json required fields drifted")


def _validate_repositories(policy: dict[str, Any]) -> None:
    repositories = policy["repositories"]
    if not isinstance(repositories, list):
        raise PolicyError("repositories must be a list")
    observed: dict[str, str] = {}
    expected_keys = {"id", "slug", "default_branch", "policy_path", "agents_path", "claude_path"}
    for index, repository in enumerate(repositories):
        if not isinstance(repository, dict):
            raise PolicyError(f"repositories[{index}] must be an object")
        _exact_keys(repository, expected_keys, f"repositories[{index}]")
        repo_id = repository["id"]
        if repo_id in observed:
            raise PolicyError(f"duplicate repository id: {repo_id}")
        observed[repo_id] = repository["slug"]
        if repository["default_branch"] != "main":
            raise PolicyError(f"repository {repo_id} must publish from main")
        expected_paths = (".github/release-policy.json", "AGENTS.md", "CLAUDE.md")
        actual_paths = (repository["policy_path"], repository["agents_path"], repository["claude_path"])
        if actual_paths != expected_paths:
            raise PolicyError(f"repository {repo_id} has unknown adapter paths")
    if observed != REPOSITORIES:
        raise PolicyError(f"repository registry drifted: {observed}")


def _validate_branches_checks_profiles(policy: dict[str, Any]) -> None:
    branch_classes = policy["branch_classes"]
    if not isinstance(branch_classes, dict) or set(branch_classes) != BRANCH_CLASSES:
        raise PolicyError(f"unknown branch class registry: {sorted(branch_classes)}")
    for name, pattern in branch_classes.items():
        if not isinstance(pattern, str):
            raise PolicyError(f"branch class {name} must be a regex")
        try:
            re.compile(pattern)
        except re.error as exc:
            raise PolicyError(f"invalid regex for branch class {name}: {exc}") from exc

    checks = policy["checks"]
    if not isinstance(checks, dict) or set(checks) != CHECKS:
        raise PolicyError(f"unknown check registry: {sorted(set(checks) ^ CHECKS)}")
    for name, check in checks.items():
        if not isinstance(check, dict):
            raise PolicyError(f"check {name} must be an object")
        _exact_keys(check, {"scope"}, f"checks.{name}")
        if check["scope"] not in {"all_prs", "all_prs_and_promotions", "changed_components", "promotions"}:
            raise PolicyError(f"check {name} has unknown scope")

    profiles = policy["ci_profiles"]
    if not isinstance(profiles, dict) or set(profiles) != PROFILES:
        raise PolicyError("ci_profiles must contain exactly quick and full")
    for name, profile in profiles.items():
        if not isinstance(profile, dict):
            raise PolicyError(f"profile {name} must be an object")
        _exact_keys(profile, {"checks"}, f"ci_profiles.{name}")
        _known_refs(profile["checks"], CHECKS, f"ci_profiles.{name}.checks")


def _validate_roles_transitions(policy: dict[str, Any]) -> None:
    roles = policy["roles"]
    if not isinstance(roles, dict) or set(roles) != ROLES:
        raise PolicyError(f"unknown role registry: {sorted(set(roles) ^ ROLES)}")
    for name, role in roles.items():
        if not isinstance(role, dict):
            raise PolicyError(f"role {name} must be an object")
        _exact_keys(role, {"actors", "allow", "deny"}, f"roles.{name}")
        _known_refs(role["actors"], ACTORS, f"roles.{name}.actors")
        allowed = _string_list(role["allow"], f"roles.{name}.allow")
        denied = _string_list(role["deny"], f"roles.{name}.deny")
        if set(allowed) & set(denied):
            raise PolicyError(f"role {name} both allows and denies an action")

    transitions = policy["transitions"]
    if not isinstance(transitions, list):
        raise PolicyError("transitions must be a list")
    observed: set[str] = set()
    transition_keys = {"id", "head", "base", "roles", "ci_profile", "required_checks", "forbidden_changes", "requires_lease"}
    for index, transition in enumerate(transitions):
        if not isinstance(transition, dict):
            raise PolicyError(f"transitions[{index}] must be an object")
        _exact_keys(transition, transition_keys, f"transitions[{index}]")
        transition_id = transition["id"]
        if transition_id not in TRANSITIONS or transition_id in observed:
            raise PolicyError(f"unknown or duplicate transition: {transition_id}")
        observed.add(transition_id)
        _known_refs(transition["head"], BRANCH_CLASSES, f"transitions.{transition_id}.head")
        _known_refs(transition["base"], BRANCH_CLASSES, f"transitions.{transition_id}.base")
        _known_refs(transition["roles"], ROLES, f"transitions.{transition_id}.roles")
        _known_refs(transition["required_checks"], CHECKS, f"transitions.{transition_id}.required_checks")
        _string_list(transition["forbidden_changes"], f"transitions.{transition_id}.forbidden_changes")
        if transition["ci_profile"] not in PROFILES:
            raise PolicyError(f"transition {transition_id} has unknown profile")
        if transition["requires_lease"] is not True:
            raise PolicyError(f"transition {transition_id} must require a lease")
    if observed != TRANSITIONS:
        raise PolicyError(f"missing transitions: {sorted(TRANSITIONS - observed)}")


def _validate_agents_release(policy: dict[str, Any]) -> None:
    agents = policy["agents"]
    _exact_keys(agents, {"providers", "overrides", "lease"}, "agents")
    providers = agents["providers"]
    if set(providers) != {"openai", "anthropic"}:
        raise PolicyError("agents.providers must contain OpenAI and Anthropic")
    expected_adapters = {"openai": "AGENTS.md", "anthropic": "CLAUDE.md"}
    for provider, adapter in expected_adapters.items():
        value = providers[provider]
        _exact_keys(value, {"adapter", "role"}, f"agents.providers.{provider}")
        if value != {"adapter": adapter, "role": "fix_agent"}:
            raise PolicyError(f"provider {provider} must have the same fix_agent permissions")

    overrides = agents["overrides"]
    _exact_keys(overrides, {"paths", "effect", "managed_runners_fail_on_widening"}, "agents.overrides")
    _string_list(overrides["paths"], "agents.overrides.paths", nonempty=True)
    if overrides["effect"] != "tighten_only" or overrides["managed_runners_fail_on_widening"] is not True:
        raise PolicyError("agent overrides must only tighten the policy")

    lease = agents["lease"]
    lease_keys = {"version", "lock_label_prefix", "visible_label", "required_fields", "max_ttl_minutes", "handoff_requires_comment", "concurrent_acquire"}
    _exact_keys(lease, lease_keys, "agents.lease")
    required_lease_fields = {"provider", "run_id", "issue", "branch", "base_sha", "acquired_at", "expires_at", "heartbeat_at"}
    if set(_string_list(lease["required_fields"], "agents.lease.required_fields")) != required_lease_fields:
        raise PolicyError("lease required fields drifted")
    if not 1 <= lease["max_ttl_minutes"] <= 480:
        raise PolicyError("lease max TTL must be between 1 and 480 minutes")
    if lease["concurrent_acquire"] != "fail_closed" or lease["handoff_requires_comment"] is not True:
        raise PolicyError("lease concurrency or handoff is not fail-closed")

    release = policy["release"]
    _exact_keys(release, {"manifest_components", "required_component_fields", "forbidden_floating_refs", "tagging", "publishing"}, "release")
    if set(_string_list(release["manifest_components"], "release.manifest_components")) != set(REPOSITORIES):
        raise PolicyError("release manifest must pin all four repositories")
    if set(_string_list(release["required_component_fields"], "release.required_component_fields")) != {"git_sha", "immutable_url", "sha256"}:
        raise PolicyError("release component identity fields drifted")
    forbidden_refs = set(_string_list(release["forbidden_floating_refs"], "release.forbidden_floating_refs"))
    if not {"main", "latest", "releases/latest"} <= forbidden_refs:
        raise PolicyError("release floating ref deny-list is incomplete")

    tagging = release["tagging"]
    _exact_keys(tagging, {"source_branch", "pattern", "automatic_only", "role", "requires_checks"}, "release.tagging")
    if tagging["source_branch"] != "main" or tagging["automatic_only"] is not True:
        raise PolicyError("release tags must be created automatically from main")
    if tagging["role"] != "release_controller":
        raise PolicyError("only release_controller may tag")
    _known_refs(tagging["requires_checks"], CHECKS, "release.tagging.requires_checks")
    try:
        re.compile(tagging["pattern"])
    except re.error as exc:
        raise PolicyError(f"invalid release tag pattern: {exc}") from exc

    publishing = release["publishing"]
    _exact_keys(publishing, {"build_before_promote", "idempotent_retry_without_new_tag", "stable_channels"}, "release.publishing")
    if publishing["build_before_promote"] is not True or publishing["idempotent_retry_without_new_tag"] is not True:
        raise PolicyError("publication must stage first and retry without a new tag")
    _string_list(publishing["stable_channels"], "release.publishing.stable_channels", nonempty=True)


def _validate_changes_ownership(policy: dict[str, Any]) -> None:
    changes = policy["changes"]
    _exact_keys(changes, {"ci_full_patterns", "unit_pr_forbidden_patterns"}, "changes")
    ci_full_patterns = set(_string_list(changes["ci_full_patterns"], "changes.ci_full_patterns", nonempty=True))
    required_full_patterns = {".github/**", "AGENTS.md", "CLAUDE.md", "scripts/*policy*", "scripts/*lease*"}
    if not required_full_patterns <= ci_full_patterns:
        raise PolicyError("ci:full patterns do not cover policy and agent control files")
    forbidden_patterns = set(
        _string_list(changes["unit_pr_forbidden_patterns"], "changes.unit_pr_forbidden_patterns", nonempty=True)
    )
    if "refs/tags/v*" not in forbidden_patterns:
        raise PolicyError("unit PR forbidden patterns must cover release tags")

    ownership = policy["ownership"]
    _exact_keys(ownership, {"owners", "protected_paths"}, "ownership")
    if _string_list(ownership["owners"], "ownership.owners", nonempty=True) != ["@renesenses"]:
        raise PolicyError("unknown policy owner registry")
    protected_paths = set(_string_list(ownership["protected_paths"], "ownership.protected_paths", nonempty=True))
    required_paths = {
        ".github/release-policy.json",
        ".github/release-policy.schema.json",
        ".github/release-policy.sha256",
        ".github/workflows/**",
        ".github/CODEOWNERS",
        "AGENTS.md",
        "CLAUDE.md",
        "scripts/*policy*",
        "scripts/*lease*",
    }
    if not required_paths <= protected_paths:
        raise PolicyError("ownership does not cover all policy control paths")


def _validate_supporting_files(root: Path) -> None:
    agents_path = root / "AGENTS.md"
    claude_path = root / "CLAUDE.md"
    try:
        agents_text = agents_path.read_text(encoding="utf-8")
        claude_text = claude_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise PolicyError(f"cannot read agent adapter: {exc}") from exc

    for marker in ("POLICY_PATH:", "POLICY_DIGEST_PATH:", "scripts/agent_lease.py"):
        if marker not in agents_text:
            raise PolicyError(f"AGENTS.md is missing {marker}")
    if "release/v0.9" in agents_text:
        raise PolicyError("AGENTS.md still contains the legacy release branch doctrine")

    first_line = next((line.strip() for line in claude_text.splitlines() if line.strip()), "")
    if first_line != "@AGENTS.md":
        raise PolicyError("CLAUDE.md must import @AGENTS.md first")
    if claude_text.count("@AGENTS.md") != 1 or "CLAUDE_ADAPTER_ONLY: true" not in claude_text:
        raise PolicyError("CLAUDE.md must remain a thin Claude-only adapter")
    if "POLICY_DIGEST_PATH:" in claude_text or "release/v0.9" in claude_text:
        raise PolicyError("CLAUDE.md copies or contradicts the common contract")

    required_codeowners = {
        "/.github/release-policy.json @renesenses",
        "/.github/workflows/ @renesenses",
        "/AGENTS.md @renesenses",
        "/CLAUDE.md @renesenses",
    }
    codeowners_text = (root / ".github/CODEOWNERS").read_text(encoding="utf-8")
    if not required_codeowners <= set(codeowners_text.splitlines()):
        raise PolicyError("CODEOWNERS does not protect policy and adapters")

    template_text = (root / ".github/pull_request_template.md").read_text(encoding="utf-8")
    for heading in ("## Traçabilité", "## Preuves", "## Ce qui n'est PAS traité"):
        if heading not in template_text:
            raise PolicyError(f"pull request template is missing {heading}")

    legacy_runbook = root / "docs/RELEASE-OPERATIONS.md"
    if legacy_runbook.exists():
        prefix = legacy_runbook.read_text(encoding="utf-8")[:800]
        if "RELEASE_RUNBOOK_STATUS: legacy-disabled" not in prefix:
            raise PolicyError("legacy release runbook is not explicitly disabled")


def _validate_digest(root: Path) -> str:
    policy_bytes = (root / POLICY_REL).read_bytes()
    observed = hashlib.sha256(policy_bytes).hexdigest()
    try:
        digest_line = (root / DIGEST_REL).read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise PolicyError(f"cannot read policy digest: {exc}") from exc
    match = re.fullmatch(r"([0-9a-f]{64})  \.github/release-policy\.json", digest_line)
    if not match:
        raise PolicyError("release-policy.sha256 has an invalid format")
    expected = match.group(1)
    if observed != expected:
        raise PolicyError(f"policy digest mismatch: expected {expected}, observed {observed}")
    return observed


def validate(root: Path) -> tuple[dict[str, Any], str]:
    root = root.resolve()
    policy = _load_json(root / POLICY_REL)
    _exact_keys(policy, TOP_LEVEL_KEYS, "policy")
    if policy["schema_version"] != 1:
        raise PolicyError(f"unknown schema version: {policy['schema_version']}")
    if not isinstance(policy["policy_version"], str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", policy["policy_version"]):
        raise PolicyError("policy_version must be semantic versioning")
    if policy["mode"] not in {"migration_freeze", "enforced"}:
        raise PolicyError(f"unknown policy mode: {policy['mode']}")
    if policy["$schema"] != "./release-policy.schema.json":
        raise PolicyError("policy references an unknown schema")

    _validate_schema_file(root)
    _validate_repositories(policy)
    _validate_branches_checks_profiles(policy)
    _validate_roles_transitions(policy)
    _validate_agents_release(policy)
    _validate_changes_ownership(policy)
    _validate_supporting_files(root)
    digest = _validate_digest(root)
    return policy, digest


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("validate", "identity"))
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        policy, digest = validate(args.root)
    except (PolicyError, OSError) as exc:
        print(f"release-policy: FAIL: {exc}", file=sys.stderr)
        return 1
    identity = f"version={policy['policy_version']} schema={policy['schema_version']} mode={policy['mode']} sha256={digest}"
    if args.command == "identity":
        print(identity)
    else:
        print(f"release-policy: OK: {identity}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
