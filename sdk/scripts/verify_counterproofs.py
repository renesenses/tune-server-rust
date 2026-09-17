#!/usr/bin/env python3
"""Behavioral mutations on our SDK sources and a generated scratch plugin.

Run serially on an isolated checkout, with no concurrent edits. Each source is
backed up and restored by copying in finally, then the unchanged witness is run
again. Compilation failure is NEVER accepted as the expected red.
"""
import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile


def run(command):
    print("COMMAND", repr([str(arg) for arg in command]), flush=True)
    result = subprocess.run([str(arg) for arg in command], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end="", flush=True)
    return result


def mutate(path, old, new, command, witness, scratch):
    source = path.read_text()
    if source.count(old) != 1:
        raise AssertionError(f"mutation site ambiguous: {path}")
    # Cargo freshness uses mtimes. A source copied from an older snapshot must
    # not accidentally execute a previously mutated build artifact.
    path.touch()
    baseline = run(command)
    if baseline.returncode != 0 or "1 passed" not in baseline.stdout:
        raise AssertionError(f"baseline witness did not run green: {witness}")
    backup = scratch / "source.backup"
    shutil.copy2(path, backup)
    try:
        path.write_text(source.replace(old, new))
        red = run(command)
        if red.returncode != 101 or f"{witness} ... FAILED" not in red.stdout or "error[E" in red.stdout:
            raise AssertionError(f"mutation was not a behavioral failure of {witness}")
    finally:
        # copyfile restores the bytes with a fresh mtime. copy2 would restore
        # the OLD mtime and Cargo could reuse the mutant binary on the green run.
        shutil.copyfile(backup, path)
    restored = run(command)
    if restored.returncode != 0 or "1 passed" not in restored.stdout:
        raise AssertionError(f"restored witness did not run green: {witness}")
    print(f"COUNTERPROOF VERIFIED: {witness}", flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--sdk", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    sdk = args.sdk.resolve()
    with tempfile.TemporaryDirectory(prefix="tune-sdk-counterproof-") as temp:
        scratch = Path(temp)
        for path, old, new, witness in [
            ("manifest.rs", "} else if c.required {", "} else if false && c.required {", "missing_required_capability_is_rejected_before_setup"),
            ("observation.rs", "let n = self.dbfs.len();", "if self.dbfs.is_empty() { return Ok(()); }\n        let n = self.dbfs.len();", "spectrum_requires_real_axes_resolution_and_measurement_provenance"),
        ]:
            command = ["cargo", "test", "--manifest-path", sdk / "Cargo.toml", "--locked", "-p", "tune-plugin-sdk", "--test", "contracts", witness, "--", "--exact"]
            mutate(sdk / "tune-plugin-sdk/src" / path, old, new, command, witness, scratch)
        plugin = scratch / "generated"
        generated = run([args.binary.resolve(), "new", "counterproof", "--template", "dsp", "--sdk-path", sdk, "--output", plugin])
        if generated.returncode:
            raise AssertionError("scaffolding failed")
        witness = "gain_reaches_captured_samples_across_block_sizes"
        command = ["cargo", "test", "--manifest-path", plugin / "Cargo.toml", "--test", "conformance", witness, "--", "--exact"]
        mutate(plugin / "src/lib.rs", "*sample *= self.gain;", "*sample *= 1.0;", command, witness, scratch)


if __name__ == "__main__":
    main()
