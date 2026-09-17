#!/usr/bin/env python3
"""Validate inventory and witness references, not runtime conformance."""
import hashlib
import json
from pathlib import Path
import re
import subprocess

root = Path(__file__).resolve().parents[2]
matrix = json.loads((root / "docs/plugins/premium-sdk-matrix.json").read_text())
assert matrix["schema_version"] == 1
assert matrix["sdk_status"] == "experimental"
assert set(matrix["features"]) == {"equalizer", "crossfeed", "converter", "declick", "shared"}
ids = set()
for row in matrix["requirements"]:
    assert row["id"] not in ids, f"duplicate requirement: {row['id']}"
    ids.add(row["id"])
    assert row["feature"] in matrix["features"]
    assert row["production_status"] == "pending", "this first tranche cannot certify production migration"
    assert row["sdk_status"] in {"contract_test", "planned"}
    assert row["requirement"].strip()
    assert row["baseline_files"]
    for path in row["baseline_files"]:
        assert path in matrix["baseline"]["files"], f"baseline not pinned: {path}"
    if row["sdk_status"] == "contract_test":
        assert row["witnesses"], f"no witnesses for {row['id']}"
    for witness in row["witnesses"]:
        path, name = witness.split("::")
        text = (root / path).read_text()
        assert re.search(r"#\[test\]\s*fn\s+" + re.escape(name) + r"\s*\(", text), f"missing test: {witness}"

for path, expected in matrix["baseline"]["files"].items():
    data = subprocess.check_output(["git", "show", f"{matrix['baseline']['server_sha']}:{path}"], cwd=root)
    assert hashlib.sha256(data).hexdigest() == expected, f"baseline hash mismatch: {path}"

print(f"{len(ids)} requirements inventoried; source baseline and test references verified. Production: pending.")
