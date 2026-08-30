#!/usr/bin/env python3
"""Counter-examples for the canonical release policy and agent lease."""

from __future__ import annotations

import hashlib
import json
import shutil
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

from agent_lease import LeaseError, _record, acquire, handoff, heartbeat, release
from release_policy import DIGEST_REL, POLICY_REL, PolicyError, validate


SOURCE_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_FILES = (
    ".github/release-policy.json",
    ".github/release-policy.schema.json",
    ".github/release-policy.sha256",
    ".github/CODEOWNERS",
    ".github/pull_request_template.md",
    "AGENTS.md",
    "CLAUDE.md",
    "docs/RELEASE-OPERATIONS.md",
)


class PolicyFixture:
    def __init__(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        for relative in FIXTURE_FILES:
            source = SOURCE_ROOT / relative
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)

    def close(self) -> None:
        self._temp.cleanup()

    def mutate_policy(self, mutator) -> None:
        path = self.root / POLICY_REL
        policy = json.loads(path.read_text(encoding="utf-8"))
        mutator(policy)
        path.write_text(json.dumps(policy, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        (self.root / DIGEST_REL).write_text(f"{digest}  .github/release-policy.json\n", encoding="utf-8")


class ReleasePolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = PolicyFixture()

    def tearDown(self) -> None:
        self.fixture.close()

    def test_canonical_policy_is_valid(self) -> None:
        policy, digest = validate(self.fixture.root)
        self.assertEqual(policy["policy_version"], "1.0.0")
        self.assertEqual(len(digest), 64)

    def test_unknown_role_is_fail_closed(self) -> None:
        self.fixture.mutate_policy(lambda policy: policy["transitions"][0].update(roles=["ghost_agent"]))
        with self.assertRaisesRegex(PolicyError, "unknown values"):
            validate(self.fixture.root)

    def test_unknown_check_is_fail_closed(self) -> None:
        self.fixture.mutate_policy(lambda policy: policy["transitions"][0].update(required_checks=["policy-gate", "invented-check"]))
        with self.assertRaisesRegex(PolicyError, "unknown values"):
            validate(self.fixture.root)

    def test_unknown_branch_class_is_fail_closed(self) -> None:
        self.fixture.mutate_policy(lambda policy: policy["transitions"][0].update(base=["staging"]))
        with self.assertRaisesRegex(PolicyError, "unknown values"):
            validate(self.fixture.root)

    def test_unknown_schema_version_is_fail_closed(self) -> None:
        self.fixture.mutate_policy(lambda policy: policy.update(schema_version=2))
        with self.assertRaisesRegex(PolicyError, "unknown schema version"):
            validate(self.fixture.root)

    def test_unknown_nested_policy_field_is_fail_closed(self) -> None:
        self.fixture.mutate_policy(lambda policy: policy["changes"].update(temporary_bypass=True))
        with self.assertRaisesRegex(PolicyError, "unknown"):
            validate(self.fixture.root)

    def test_openai_and_anthropic_cannot_receive_different_roles(self) -> None:
        self.fixture.mutate_policy(lambda policy: policy["agents"]["providers"]["anthropic"].update(role="integrator"))
        with self.assertRaisesRegex(PolicyError, "same fix_agent permissions"):
            validate(self.fixture.root)

    def test_claude_cannot_copy_the_common_contract(self) -> None:
        agents = (self.fixture.root / "AGENTS.md").read_text(encoding="utf-8")
        (self.fixture.root / "CLAUDE.md").write_text("@AGENTS.md\nCLAUDE_ADAPTER_ONLY: true\n" + agents, encoding="utf-8")
        with self.assertRaisesRegex(PolicyError, "copies or contradicts"):
            validate(self.fixture.root)

    def test_legacy_runbook_must_be_disabled(self) -> None:
        path = self.fixture.root / "docs/RELEASE-OPERATIONS.md"
        path.write_text(path.read_text(encoding="utf-8").replace("RELEASE_RUNBOOK_STATUS: legacy-disabled", ""), encoding="utf-8")
        with self.assertRaisesRegex(PolicyError, "not explicitly disabled"):
            validate(self.fixture.root)

    def test_digest_mismatch_is_fail_closed(self) -> None:
        (self.fixture.root / POLICY_REL).write_text("{}\n", encoding="utf-8")
        with self.assertRaises(PolicyError):
            validate(self.fixture.root)


class FakeBackend:
    def __init__(self) -> None:
        self.labels: dict[str, str] = {}
        self.issue_labels: dict[int, set[str]] = {}
        self.comments: list[tuple[int, str]] = []

    def create_label(self, repo: str, name: str, description: str) -> bool:
        del repo
        if name in self.labels:
            return False
        self.labels[name] = description
        return True

    def get_label_description(self, repo: str, name: str) -> str | None:
        del repo
        return self.labels.get(name)

    def edit_label(self, repo: str, name: str, description: str) -> None:
        del repo
        if name not in self.labels:
            raise LeaseError("missing label")
        self.labels[name] = description

    def delete_label(self, repo: str, name: str) -> None:
        del repo
        del self.labels[name]

    def add_issue_labels(self, repo: str, issue: int, labels: list[str]) -> None:
        del repo
        self.issue_labels.setdefault(issue, set()).update(labels)

    def remove_issue_label(self, repo: str, issue: int, label: str) -> None:
        del repo
        self.issue_labels.setdefault(issue, set()).discard(label)

    def comment(self, repo: str, issue: int, body: str) -> None:
        del repo
        self.comments.append((issue, body))


class AgentLeaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = FakeBackend()
        self.base_sha = "a" * 40
        self.now = datetime(2026, 8, 30, 12, 0, tzinfo=timezone.utc)

    def record(self, provider: str, run_id: str, action: str = "acquire"):
        return _record(
            action=action,
            provider=provider,
            run_id=run_id,
            issue=2812,
            branch="fix/2812-policy",
            base_sha=self.base_sha,
            ttl_minutes=60,
            max_ttl_minutes=480,
            now=self.now,
        )

    def test_two_providers_cannot_acquire_the_same_issue(self) -> None:
        acquire(
            self.backend,
            repo="renesenses/tune-server-rust",
            visible_label="en-cours",
            label_prefix="verrou:issue-",
            record=self.record("openai", "codex-run"),
        )
        with self.assertRaisesRegex(LeaseError, "already leased"):
            acquire(
                self.backend,
                repo="renesenses/tune-server-rust",
                visible_label="en-cours",
                label_prefix="verrou:issue-",
                record=self.record("anthropic", "claude-run"),
            )

    def test_handoff_changes_owner_without_unlocking(self) -> None:
        acquire(
            self.backend,
            repo="renesenses/tune-server-rust",
            visible_label="en-cours",
            label_prefix="verrou:issue-",
            record=self.record("openai", "codex-run"),
        )
        handoff(
            self.backend,
            repo="renesenses/tune-server-rust",
            label_prefix="verrou:issue-",
            from_provider="openai",
            from_run_id="codex-run",
            record=self.record("anthropic", "claude-run", action="handoff"),
        )
        self.assertIn("owner=anthropic/claude-run", self.backend.labels["verrou:issue-2812"])
        release(
            self.backend,
            repo="renesenses/tune-server-rust",
            visible_label="en-cours",
            label_prefix="verrou:issue-",
            issue=2812,
            provider="anthropic",
            run_id="claude-run",
        )
        self.assertNotIn("verrou:issue-2812", self.backend.labels)
        self.assertNotIn("en-cours", self.backend.issue_labels[2812])

    def test_heartbeat_preserves_acquisition_time(self) -> None:
        acquire(
            self.backend,
            repo="renesenses/tune-server-rust",
            visible_label="en-cours",
            label_prefix="verrou:issue-",
            record=self.record("openai", "codex-run"),
        )
        later = _record(
            action="heartbeat",
            provider="openai",
            run_id="codex-run",
            issue=2812,
            branch="fix/2812-policy",
            base_sha=self.base_sha,
            ttl_minutes=60,
            max_ttl_minutes=480,
            now=self.now + timedelta(minutes=30),
        )
        heartbeat(
            self.backend,
            repo="renesenses/tune-server-rust",
            label_prefix="verrou:issue-",
            record=later,
        )
        description = self.backend.labels["verrou:issue-2812"]
        self.assertIn("acq=20260830T120000Z", description)
        self.assertIn("exp=20260830T133000Z", description)


if __name__ == "__main__":
    unittest.main(verbosity=2)
