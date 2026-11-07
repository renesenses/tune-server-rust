#!/usr/bin/env python3
"""Build the pinned AirPlay 2 helper (`airplay-daemon`) for one target (#8112).

One recipe for every shipped platform, native or cross: release.yml (each
archive), docker.yml (amd64 and arm64 image) and a local check on Shrek.

Until #8112 the arm64 Linux targets were skipped in release.yml ("Cross targets
skipped") and docker.yml staged an EMPTY file for arm64. The Tune OS Raspberry
Pi image installs the linux-aarch64 archive as is, so a Pi only spoke AirPlay 1
and AirPlay-2-only speakers answered 403 `airplay2_pairing_required`.

`airplay-daemon` needs no system library: its only native code is the vendored
fdk-aac (C++, built by `cc`) and `ring`. The cross container therefore only
needs its own C/C++ toolchain. We pass our OWN `Cross.toml`
(packaging/airplay-daemon/Cross.toml) through CROSS_CONFIG: the fork's file
installs `libfdk-aac-dev`, avahi and dbus for the target, none of which the
daemon links, and an apt failure there would fail the build for nothing.

Any failure exits non-zero, and the output directory never holds a stale or
empty binary: a 0-byte file would make `daemon_available()` look further and
an empty stub in an archive is worse than no helper at all.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

REPO = "https://github.com/renesenses/airplay2-rs"
# Pinned rev of our fork. Same rev as Dockerfile (local developer image).
REV = "83a7def6ab39d3709fa47a40eb90993b64afe9b4"
ROOT = Path(__file__).resolve().parent.parent

# ELF e_machine values (little-endian u16 at offset 18).
ELF_MACHINES = {"x86_64": 0x3E, "aarch64": 0xB7}


def check_binary(binary, target):
    """Read the file the linker produced; never execute a foreign target."""
    if not binary.is_file() or binary.stat().st_size == 0:
        raise RuntimeError(f"airplay-daemon build produced no executable for {target}")
    head = binary.read_bytes()[:64]
    if "linux" in target:
        arch = target.split("-")[0]
        if head[:4] != b"\x7fELF":
            raise RuntimeError(f"{binary} is not an ELF file")
        machine = int.from_bytes(head[18:20], "little")
        if machine != ELF_MACHINES[arch]:
            raise RuntimeError(f"{binary} is ELF machine {machine:#x}, expected {arch}")
    elif "windows" in target:
        if head[:2] != b"MZ":
            raise RuntimeError(f"{binary} is not a PE executable")
    elif "darwin" in target:
        if head[:4] not in (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe"):
            raise RuntimeError(f"{binary} is not a Mach-O executable")


def fetch(work):
    source = work / "airplay2-rs"
    if source.exists():
        shutil.rmtree(source)
    subprocess.run(["git", "init", "-q", str(source)], check=True)
    subprocess.run(["git", "fetch", "-q", "--depth", "1", REPO, REV], cwd=source, check=True)
    subprocess.run(["git", "checkout", "-q", "FETCH_HEAD"], cwd=source, check=True)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, check=True,
                          capture_output=True, text=True).stdout.strip()
    if head != REV:
        raise RuntimeError(f"airplay2-rs checkout is {head}, expected {REV}")
    return source


def build(target, output, work, tool="cargo", jobs=None):
    output, work = Path(output).resolve(), Path(work).resolve()
    # A failed rebuild must never leave a stale helper ready to package.
    if output.exists():
        shutil.rmtree(output)
    work.mkdir(parents=True, exist_ok=True)
    source = fetch(work)
    zig_target = target
    command = {"cargo": ["cargo", "build"], "cross": ["cross", "build"],
               "zigbuild": ["cargo", "zigbuild"]}[tool]
    if tool == "zigbuild" and target.endswith("-linux-gnu"):
        # Same glibc floor as the cross image (Ubuntu 20.04), runs on Debian
        # bookworm and Raspberry Pi OS.
        zig_target = target + ".2.31"
    args = [*command, "--release", "--locked", "-p", "airplay-daemon", "--target", zig_target]
    if jobs:
        args += ["--jobs", str(jobs)]
    build_env = os.environ.copy()
    if tool == "cross":
        build_env["CROSS_CONFIG"] = str(ROOT / "packaging/airplay-daemon/Cross.toml")
    if "windows-msvc" in target:
        build_env["RUSTFLAGS"] = build_env.get("RUSTFLAGS", "") + " -C target-feature=+crt-static"
    subprocess.run(args, cwd=source, env=build_env, check=True)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", source / "target"))
    name = "airplay-daemon.exe" if "windows" in target else "airplay-daemon"
    binary = target_dir / target / "release" / name
    check_binary(binary, target)
    (output / "bin").mkdir(parents=True)
    shutil.copy2(binary, output / "bin" / name)
    os.chmod(output / "bin" / name, 0o755)
    print(f"airplay-daemon {REV[:12]} built for {target}: "
          f"{(output / 'bin' / name).stat().st_size} bytes")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--work", required=True)
    parser.add_argument("--cross", action="store_true", help="build with `cross` (CI arm64)")
    parser.add_argument("--zigbuild", action="store_true", help="build with cargo-zigbuild (Shrek)")
    parser.add_argument("--jobs", type=int)
    opts = parser.parse_args()
    if opts.cross and opts.zigbuild:
        parser.error("--cross and --zigbuild are exclusive")
    build(opts.target, opts.output, opts.work,
          "cross" if opts.cross else "zigbuild" if opts.zigbuild else "cargo", opts.jobs)
