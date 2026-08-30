#!/usr/bin/env python3
"""Acquire and manage the provider-neutral Tune issue lease."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import asdict, dataclass, replace
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Protocol
from urllib.parse import quote

from release_policy import PolicyError, validate


MARKER = "<!-- tune-agent-lease:v1 -->"
PROVIDERS = {"openai", "anthropic", "human"}


class LeaseError(RuntimeError):
    """A fail-closed lease operation error."""


class Backend(Protocol):
    def create_label(self, repo: str, name: str, description: str) -> bool: ...
    def get_label_description(self, repo: str, name: str) -> str | None: ...
    def edit_label(self, repo: str, name: str, description: str) -> None: ...
    def delete_label(self, repo: str, name: str) -> None: ...
    def add_issue_labels(self, repo: str, issue: int, labels: list[str]) -> None: ...
    def remove_issue_label(self, repo: str, issue: int, label: str) -> None: ...
    def comment(self, repo: str, issue: int, body: str) -> None: ...


class GhBackend:
    """GitHub CLI backend; label creation is the atomic test-and-set."""

    @staticmethod
    def _run(args: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(args, text=True, capture_output=True, check=False)
        if check and result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip() or "unknown gh failure"
            raise LeaseError(detail)
        return result

    def create_label(self, repo: str, name: str, description: str) -> bool:
        result = self._run(
            ["gh", "label", "create", name, "--repo", repo, "--color", "5319E7", "--description", description],
            check=False,
        )
        return result.returncode == 0

    def get_label_description(self, repo: str, name: str) -> str | None:
        endpoint = f"repos/{repo}/labels/{quote(name, safe='')}"
        result = self._run(["gh", "api", endpoint, "--jq", ".description"], check=False)
        if result.returncode != 0:
            return None
        return result.stdout.strip()

    def edit_label(self, repo: str, name: str, description: str) -> None:
        self._run(["gh", "label", "edit", name, "--repo", repo, "--description", description])

    def delete_label(self, repo: str, name: str) -> None:
        self._run(["gh", "label", "delete", name, "--repo", repo, "--yes"])

    def add_issue_labels(self, repo: str, issue: int, labels: list[str]) -> None:
        args = ["gh", "issue", "edit", str(issue), "--repo", repo]
        for label in labels:
            args.extend(["--add-label", label])
        self._run(args)

    def remove_issue_label(self, repo: str, issue: int, label: str) -> None:
        self._run(["gh", "issue", "edit", str(issue), "--repo", repo, "--remove-label", label])

    def comment(self, repo: str, issue: int, body: str) -> None:
        self._run(["gh", "issue", "comment", str(issue), "--repo", repo, "--body", body])


@dataclass(frozen=True)
class LeaseRecord:
    version: int
    action: str
    provider: str
    run_id: str
    issue: int
    branch: str
    base_sha: str
    acquired_at: str
    expires_at: str
    heartbeat_at: str

    @property
    def owner(self) -> str:
        return f"{self.provider}/{self.run_id}"

    def description(self) -> str:
        description = (
            f"lease:v1 owner={self.owner} "
            f"acq={_compact_timestamp(self.acquired_at)} exp={_compact_timestamp(self.expires_at)}"
        )
        if len(description) > 100:
            raise LeaseError("lease label description exceeds GitHub's 100-character limit")
        return description

    def comment_body(self) -> str:
        return f"{MARKER}\n```json\n{json.dumps(asdict(self), sort_keys=True)}\n```"


def _timestamp(value: datetime) -> str:
    return value.astimezone(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _compact_timestamp(value: str) -> str:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise LeaseError(f"invalid lease timestamp: {value}") from exc
    return parsed.astimezone(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _acquired_from_description(description: str) -> str:
    match = re.search(r"\bacq=(\d{8}T\d{6}Z)\b", description)
    if not match:
        raise LeaseError("lease label is missing its acquisition timestamp")
    parsed = datetime.strptime(match.group(1), "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc)
    return _timestamp(parsed)


def _validate_identity(provider: str, run_id: str, issue: int, branch: str, base_sha: str) -> None:
    if provider not in PROVIDERS:
        raise LeaseError(f"unknown provider: {provider}")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,31}", run_id):
        raise LeaseError("run_id must contain 1-32 safe characters")
    if issue <= 0:
        raise LeaseError("issue must be positive")
    if not branch or branch.startswith("-") or any(character.isspace() for character in branch):
        raise LeaseError("branch is invalid")
    if not re.fullmatch(r"[0-9a-f]{40}(?:[0-9a-f]{24})?", base_sha):
        raise LeaseError("base_sha must be a full 40- or 64-character Git SHA")


def _record(
    *,
    action: str,
    provider: str,
    run_id: str,
    issue: int,
    branch: str,
    base_sha: str,
    ttl_minutes: int,
    max_ttl_minutes: int,
    now: datetime,
) -> LeaseRecord:
    _validate_identity(provider, run_id, issue, branch, base_sha)
    if not 1 <= ttl_minutes <= max_ttl_minutes:
        raise LeaseError(f"ttl_minutes must be between 1 and {max_ttl_minutes}")
    current = _timestamp(now)
    return LeaseRecord(
        version=1,
        action=action,
        provider=provider,
        run_id=run_id,
        issue=issue,
        branch=branch,
        base_sha=base_sha,
        acquired_at=current,
        expires_at=_timestamp(now + timedelta(minutes=ttl_minutes)),
        heartbeat_at=current,
    )


def _label(issue: int, prefix: str) -> str:
    return f"{prefix}{issue}"


def _assert_owner(backend: Backend, repo: str, label: str, provider: str, run_id: str) -> str:
    description = backend.get_label_description(repo, label)
    expected = f"owner={provider}/{run_id} "
    if description is None:
        raise LeaseError(f"lease {label} does not exist")
    if expected not in description:
        raise LeaseError(f"lease {label} is held by another run: {description}")
    return description


def acquire(
    backend: Backend,
    *,
    repo: str,
    visible_label: str,
    label_prefix: str,
    record: LeaseRecord,
) -> None:
    label = _label(record.issue, label_prefix)
    if not backend.create_label(repo, label, record.description()):
        holder = backend.get_label_description(repo, label) or "holder unknown"
        raise LeaseError(f"issue #{record.issue} is already leased: {holder}")
    try:
        backend.add_issue_labels(repo, record.issue, [label, visible_label])
    except Exception:
        # The run still owns the just-created label; rollback prevents a ghost lease.
        backend.delete_label(repo, label)
        raise
    backend.comment(repo, record.issue, record.comment_body())


def heartbeat(
    backend: Backend,
    *,
    repo: str,
    label_prefix: str,
    record: LeaseRecord,
) -> None:
    label = _label(record.issue, label_prefix)
    description = _assert_owner(backend, repo, label, record.provider, record.run_id)
    record = replace(record, acquired_at=_acquired_from_description(description))
    backend.edit_label(repo, label, record.description())
    backend.comment(repo, record.issue, record.comment_body())


def handoff(
    backend: Backend,
    *,
    repo: str,
    label_prefix: str,
    from_provider: str,
    from_run_id: str,
    record: LeaseRecord,
) -> None:
    label = _label(record.issue, label_prefix)
    _assert_owner(backend, repo, label, from_provider, from_run_id)
    backend.edit_label(repo, label, record.description())
    backend.comment(repo, record.issue, record.comment_body())


def release(
    backend: Backend,
    *,
    repo: str,
    visible_label: str,
    label_prefix: str,
    issue: int,
    provider: str,
    run_id: str,
) -> None:
    label = _label(issue, label_prefix)
    _assert_owner(backend, repo, label, provider, run_id)
    backend.delete_label(repo, label)
    backend.remove_issue_label(repo, issue, visible_label)
    body = f"{MARKER}\n```json\n{json.dumps({'version': 1, 'action': 'release', 'provider': provider, 'run_id': run_id, 'issue': issue}, sort_keys=True)}\n```"
    backend.comment(repo, issue, body)


def _common_identity(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--repo", required=True)
    parser.add_argument("--issue", required=True, type=int)
    parser.add_argument("--provider", required=True, choices=sorted(PROVIDERS))
    parser.add_argument("--run-id", required=True)


def _record_identity(parser: argparse.ArgumentParser) -> None:
    _common_identity(parser)
    parser.add_argument("--branch", required=True)
    parser.add_argument("--base-sha", required=True)
    parser.add_argument("--ttl-minutes", type=int, default=240)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    subparsers = parser.add_subparsers(dest="command", required=True)
    _record_identity(subparsers.add_parser("acquire"))
    _record_identity(subparsers.add_parser("heartbeat"))
    handoff_parser = subparsers.add_parser("handoff")
    handoff_parser.add_argument("--repo", required=True)
    handoff_parser.add_argument("--issue", required=True, type=int)
    handoff_parser.add_argument("--from-provider", required=True, choices=sorted(PROVIDERS))
    handoff_parser.add_argument("--from-run-id", required=True)
    handoff_parser.add_argument("--to-provider", required=True, choices=sorted(PROVIDERS))
    handoff_parser.add_argument("--to-run-id", required=True)
    handoff_parser.add_argument("--branch", required=True)
    handoff_parser.add_argument("--base-sha", required=True)
    handoff_parser.add_argument("--ttl-minutes", type=int, default=240)
    _common_identity(subparsers.add_parser("release"))
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        policy, _ = validate(args.root)
        lease_policy = policy["agents"]["lease"]
        backend = GhBackend()
        if args.command == "release":
            release(
                backend,
                repo=args.repo,
                visible_label=lease_policy["visible_label"],
                label_prefix=lease_policy["lock_label_prefix"],
                issue=args.issue,
                provider=args.provider,
                run_id=args.run_id,
            )
        else:
            provider = args.to_provider if args.command == "handoff" else args.provider
            run_id = args.to_run_id if args.command == "handoff" else args.run_id
            record = _record(
                action=args.command,
                provider=provider,
                run_id=run_id,
                issue=args.issue,
                branch=args.branch,
                base_sha=args.base_sha,
                ttl_minutes=args.ttl_minutes,
                max_ttl_minutes=lease_policy["max_ttl_minutes"],
                now=datetime.now(timezone.utc),
            )
            if args.command == "acquire":
                acquire(
                    backend,
                    repo=args.repo,
                    visible_label=lease_policy["visible_label"],
                    label_prefix=lease_policy["lock_label_prefix"],
                    record=record,
                )
            elif args.command == "heartbeat":
                heartbeat(backend, repo=args.repo, label_prefix=lease_policy["lock_label_prefix"], record=record)
            else:
                handoff(
                    backend,
                    repo=args.repo,
                    label_prefix=lease_policy["lock_label_prefix"],
                    from_provider=args.from_provider,
                    from_run_id=args.from_run_id,
                    record=record,
                )
        print(f"agent-lease: OK: {args.command} issue=#{args.issue}")
        return 0
    except (LeaseError, PolicyError, OSError) as exc:
        print(f"agent-lease: FAIL: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
