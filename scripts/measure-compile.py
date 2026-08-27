#!/usr/bin/env python3
"""Measure Tune's Rust compilation graph without polluting worktrees.

The script records a source/target inventory for every run. Compilation
profiles use an isolated temporary Cargo target by default, run the same
command twice (cold then warm), preserve Cargo's timing reports, and remove
only the temporary target that they created themselves.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
SCHEMA_VERSION = 1


def command_output(command: list[str]) -> str:
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def git_value(*arguments: str) -> str:
    try:
        return command_output(["git", *arguments])
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def rust_line_count(directory: Path) -> tuple[int, int]:
    files = sorted(directory.rglob("*.rs")) if directory.exists() else []
    lines = 0
    for source in files:
        try:
            with source.open("rb") as handle:
                lines += sum(1 for _ in handle)
        except OSError:
            continue
    return len(files), lines


def cargo_metadata() -> dict[str, Any]:
    raw = command_output(["cargo", "metadata", "--no-deps", "--format-version", "1"])
    return json.loads(raw)


def inventory(metadata: dict[str, Any]) -> dict[str, Any]:
    packages: list[dict[str, Any]] = []
    total_rust_files = 0
    total_rust_lines = 0
    total_test_harnesses = 0
    total_dependency_edges = 0

    for package in sorted(metadata["packages"], key=lambda value: value["name"]):
        package_root = Path(package["manifest_path"]).parent
        source_files = 0
        source_lines = 0
        for source_dir in ("src", "tests", "benches", "examples"):
            files, lines = rust_line_count(package_root / source_dir)
            source_files += files
            source_lines += lines

        targets = package["targets"]
        test_harnesses = sum("test" in target["kind"] for target in targets)
        direct_dependencies = len(package["dependencies"])
        packages.append(
            {
                "name": package["name"],
                "rust_files": source_files,
                "rust_lines": source_lines,
                "direct_dependencies": direct_dependencies,
                "test_harnesses": test_harnesses,
                "targets": [
                    {"name": target["name"], "kind": target["kind"]}
                    for target in targets
                ],
                "features": sorted(package["features"]),
            }
        )
        total_rust_files += source_files
        total_rust_lines += source_lines
        total_test_harnesses += test_harnesses
        total_dependency_edges += direct_dependencies

    return {
        "package_count": len(packages),
        "rust_files": total_rust_files,
        "rust_lines": total_rust_lines,
        "direct_dependency_edges": total_dependency_edges,
        "test_harnesses": total_test_harnesses,
        "packages": packages,
    }


def host_target() -> str:
    for line in command_output(["rustc", "-vV"]).splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    raise RuntimeError("rustc -vV did not report a host target")


def profile_command(profile_name: str) -> list[str]:
    if profile_name == "ci-test":
        return [
            "cargo",
            "test",
            "--locked",
            "-p",
            "tune-core",
            "-p",
            "tune-http-types",
            "-p",
            "tune-stream-http",
            "-p",
            "tune-streaming-http",
            "-p",
            "tune-server",
            "--no-default-features",
            "--features",
            "oaat",
            "--no-run",
            "--timings",
        ]
    if profile_name == "ci-clippy":
        return [
            "cargo",
            "clippy",
            "--locked",
            "-p",
            "tune-core",
            "-p",
            "tune-http-types",
            "-p",
            "tune-stream-http",
            "-p",
            "tune-streaming-http",
            "-p",
            "tune-server",
            "--all-targets",
            "--no-default-features",
            "--features",
            "oaat,dj,karaoke,bandcamp,plugins-wasm",
            "--timings",
            "--",
            "-D",
            "clippy::correctness",
        ]
    if profile_name == "release":
        system = platform.system()
        if system == "Darwin":
            feature_arguments = [
                "--features",
                "postgres,dj,karaoke,bandcamp,plugins-wasm,audio-embedding",
            ]
        elif system == "Windows":
            feature_arguments = [
                "--no-default-features",
                "--features",
                "oaat,local-audio,asio,postgres,dj,karaoke,bandcamp,plugins-wasm,audio-embedding",
            ]
        else:
            feature_arguments = [
                "--no-default-features",
                "--features",
                "oaat,local-audio,postgres,dj,karaoke,bandcamp,plugins-wasm,audio-embedding",
            ]
        return [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--package",
            "tune-server",
            "--target",
            host_target(),
            *feature_arguments,
            "--timings",
        ]
    raise ValueError(f"unknown profile: {profile_name}")


def directory_size(directory: Path) -> int:
    if not directory.exists():
        return 0
    total = 0
    for path in directory.rglob("*"):
        try:
            if path.is_file():
                total += path.stat().st_size
        except OSError:
            continue
    return total


def copy_timing_report(target_dir: Path, output_dir: Path, step_name: str) -> str | None:
    source = target_dir / "cargo-timings" / "cargo-timing.html"
    if not source.exists():
        return None
    destination = output_dir / f"cargo-timing-{step_name}.html"
    shutil.copy2(source, destination)
    return destination.name


def run_step(
    command: list[str], target_dir: Path, output_dir: Path, step_name: str
) -> dict[str, Any]:
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(target_dir)
    log_path = output_dir / f"{step_name}.log"
    started = time.monotonic()
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
    elapsed = time.monotonic() - started
    timing_report = copy_timing_report(target_dir, output_dir, step_name)
    return {
        "name": step_name,
        "command": command,
        "seconds": round(elapsed, 3),
        "exit_code": process.returncode,
        "target_bytes": directory_size(target_dir),
        "log": log_path.name,
        "cargo_timing": timing_report,
    }


def markdown_report(report: dict[str, Any]) -> str:
    workspace = report["inventory"]
    lines = [
        "# Mesure du graphe de compilation",
        "",
        f"- Commit : `{report['git']['commit']}`",
        f"- Profil : `{report['profile']}`",
        f"- Hôte : `{report['host']['target']}`",
        f"- Rust : `{report['host']['rustc']}`",
        f"- Crates du workspace : {workspace['package_count']}",
        f"- Sources Rust : {workspace['rust_files']} fichiers / {workspace['rust_lines']} lignes",
        f"- Harnais d’intégration : {workspace['test_harnesses']}",
        f"- Arêtes de dépendances directes : {workspace['direct_dependency_edges']}",
        "",
    ]
    if report["steps"]:
        lines.extend(
            [
                "| Passe | Durée | Taille target | Résultat |",
                "|---|---:|---:|---:|",
            ]
        )
        for step in report["steps"]:
            if step.get("skipped"):
                lines.append(f"| {step['name']} | non exécutée | — | — |")
            else:
                lines.append(
                    f"| {step['name']} | {step['seconds']:.3f} s | "
                    f"{step['target_bytes'] / (1024 ** 3):.2f} Gio | {step['exit_code']} |"
                )
        lines.append("")
    lines.extend(
        [
            "## Crates locales",
            "",
            "| Crate | Lignes Rust | Dépendances directes | Harnais |",
            "|---|---:|---:|---:|",
        ]
    )
    for package in workspace["packages"]:
        lines.append(
            f"| `{package['name']}` | {package['rust_lines']} | "
            f"{package['direct_dependencies']} | {package['test_harnesses']} |"
        )
    lines.append("")
    return "\n".join(lines)


def self_test() -> None:
    assert profile_command("ci-test")[-1] == "--timings"
    assert "--all-targets" in profile_command("ci-clippy")
    assert profile_command("release")[0:2] == ["cargo", "build"]
    sample = {
        "git": {"commit": "abc"},
        "profile": "inventory",
        "host": {"target": "test-host", "rustc": "rustc test"},
        "inventory": {
            "package_count": 1,
            "rust_files": 2,
            "rust_lines": 3,
            "test_harnesses": 1,
            "direct_dependency_edges": 4,
            "packages": [
                {
                    "name": "sample",
                    "rust_lines": 3,
                    "direct_dependencies": 4,
                    "test_harnesses": 1,
                }
            ],
        },
        "steps": [{"name": "cold", "command": ["cargo"], "skipped": True}],
    }
    rendered = markdown_report(sample)
    assert "`sample`" in rendered
    assert "Harnais d’intégration : 1" in rendered
    assert "non exécutée" in rendered
    print("measure-compile: self-test passed")


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Inventory and measure Tune's Rust compilation graph"
    )
    parser.add_argument(
        "profile",
        nargs="?",
        choices=("inventory", "ci-test", "ci-clippy", "release"),
        default="inventory",
    )
    parser.add_argument("--output", type=Path, help="report directory")
    parser.add_argument("--target-dir", type=Path, help="reuse this Cargo target")
    parser.add_argument(
        "--keep-target",
        action="store_true",
        help="keep an automatically created target directory",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="record commands without running Cargo"
    )
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    if arguments.self_test:
        self_test()
        return 0

    timestamp = time.strftime("%Y%m%d-%H%M%S")
    output_dir = (arguments.output or ROOT / "target" / "compile-measures" / timestamp).resolve()
    output_dir.mkdir(parents=True, exist_ok=False)

    temporary_target = arguments.target_dir is None
    if temporary_target:
        target_dir = Path(tempfile.mkdtemp(prefix="tune-compile-target-"))
    else:
        target_dir = arguments.target_dir.resolve()
        target_dir.mkdir(parents=True, exist_ok=True)

    report: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "profile": arguments.profile,
        "git": {
            "commit": git_value("rev-parse", "HEAD"),
            "dirty": bool(git_value("status", "--porcelain")),
        },
        "host": {
            "system": platform.platform(),
            "target": host_target(),
            "rustc": command_output(["rustc", "--version"]),
            "cargo": command_output(["cargo", "--version"]),
        },
        "target_dir": str(target_dir),
        "target_temporary": temporary_target,
        "inventory": inventory(cargo_metadata()),
        "steps": [],
    }

    exit_code = 0
    try:
        if arguments.profile != "inventory":
            command = profile_command(arguments.profile)
            if arguments.dry_run:
                report["steps"] = [
                    {"name": "cold", "command": command, "skipped": True},
                    {"name": "warm", "command": command, "skipped": True},
                ]
            else:
                for step_name in ("cold", "warm"):
                    step = run_step(command, target_dir, output_dir, step_name)
                    report["steps"].append(step)
                    if step["exit_code"] != 0:
                        exit_code = step["exit_code"]
                        break
    finally:
        (output_dir / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        (output_dir / "report.md").write_text(markdown_report(report), encoding="utf-8")
        if temporary_target and not arguments.keep_target:
            shutil.rmtree(target_dir)

    print(output_dir)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
