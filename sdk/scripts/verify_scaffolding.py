#!/usr/bin/env python3
"""Generate and execute both plugins outside the SDK/server workspace.

Only stdlib. Temporary projects are scoped by TemporaryDirectory and removed
even on a failed command. Build artifacts obey the caller's CARGO_TARGET_DIR.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def run(*args, expected=0, quiet=False):
    result = subprocess.run([str(arg) for arg in args], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode != expected:
        raise AssertionError(f"{args!r}: expected exit {expected}, got {result.returncode}\n{result.stdout}")
    if not quiet:
        print(result.stdout, end="")
    return result.stdout


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--sdk", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    binary, sdk = args.binary.resolve(), args.sdk.resolve()
    with tempfile.TemporaryDirectory(prefix="tune-sdk-conformance-") as scratch:
        root = Path(scratch)
        for template in ("dsp", "batch"):
            project = root / f"external {template} plugin"
            command = (binary, "new", f"example-{template}", "--template", template, "--sdk-path", sdk, "--output", project)
            run(*command)
            run(binary, "check", project)
            metadata = json.loads(run("cargo", "metadata", "--manifest-path", project / "Cargo.toml", "--no-deps", "--format-version", "1", quiet=True))
            assert Path(metadata["workspace_root"]).resolve() == project.resolve(), "generated project must own its workspace"
            deps = {d["name"] for package in metadata["packages"] for d in package["dependencies"]}
            assert not deps.intersection({"tune-core", "tune-server"}), "plugin must depend only on public SDK crates"
            run(binary, "test", project)
            before = (project / "src/lib.rs").read_bytes()
            run(*command, expected=1)
            assert (project / "src/lib.rs").read_bytes() == before, "scaffolding must not overwrite a project"
            manifest = json.loads((project / "manifest.json").read_text())
            manifest["sdk"]["minor"] = 999
            (project / "manifest.json").write_text(json.dumps(manifest))
            run(binary, "check", project, expected=1)
        run(binary, "new", "../escape", "--template", "dsp", "--sdk-path", sdk, "--output", root / "escape", expected=1)
        assert not (root / "escape").exists()
    print("External DSP and batch scaffolding verified; production host, codecs and UI are not exercised.")


if __name__ == "__main__":
    main()
