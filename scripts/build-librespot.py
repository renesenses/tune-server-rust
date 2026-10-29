#!/usr/bin/env python3
"""Build the pinned standalone Spotify helper, with its dependency notices (#6872)."""
import argparse
import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

VERSION = "0.8.0"
SHA256 = "030c5e98cc06f20283b5948ae23bdac1de0dec58dee2c5ec4c8dafc7aaa796ab"
FEATURES = "rustls-tls-webpki-roots,with-libmdns"
ROOT = Path(__file__).resolve().parent.parent


def build(target, output, work, cross=False):
    output, work = Path(output).resolve(), Path(work).resolve()
    # A failed rebuild must never leave a stale helper ready to package.
    if output.exists():
        shutil.rmtree(output)
    work.mkdir(parents=True, exist_ok=True)
    url = f"https://static.crates.io/crates/librespot/librespot-{VERSION}.crate"
    with urllib.request.urlopen(url, timeout=120) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise RuntimeError("librespot source checksum mismatch")
    source = work / f"librespot-{VERSION}"
    if source.exists():
        shutil.rmtree(source)
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        archive.extractall(work, filter="data")
    # Build outside Tune: no server workspace, features or ALSA pre-build hooks.
    args = ["build", "--release", "--locked", "--bin", "librespot", "--target", target,
            "--no-default-features", "--features", FEATURES, "--jobs", "4"]
    build_env = os.environ.copy()
    if "windows-msvc" in target:
        build_env["RUSTFLAGS"] = build_env.get("RUSTFLAGS", "") + " -C target-feature=+crt-static"
    subprocess.run(["cross" if cross else "cargo", *args], cwd=source, env=build_env, check=True)
    # Never execute a foreign target on the host. The linker produced this file.
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", source / "target"))
    binary = target_dir / target / "release" / ("librespot.exe" if "windows" in target else "librespot")
    if not binary.is_file() or binary.stat().st_size == 0:
        raise RuntimeError("librespot build produced no executable")
    notices = work / "LIBRESPOT-THIRD-PARTY.txt"
    subprocess.run(["cargo", "about", "generate", "--locked", "--fail", "--target", target,
                    "--no-default-features", "--features", FEATURES.replace(",", " "),
                    "--config", str(ROOT / "packaging/librespot/about.toml"),
                    "--output-file", str(notices),
                    str(ROOT / "packaging/librespot/notices.hbs")], cwd=source, check=True)
    if not notices.is_file() or notices.stat().st_size == 0:
        raise RuntimeError("librespot dependency notices missing")
    output.mkdir(parents=True)
    (output / "bin").mkdir()
    shutil.copy2(binary, output / "bin" / binary.name)
    shutil.copy2(source / "LICENSE", output / "LIBRESPOT-LICENSE.txt")
    shutil.copy2(notices, output / notices.name)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--work", required=True)
    parser.add_argument("--cross", action="store_true")
    opts = parser.parse_args()
    build(opts.target, opts.output, opts.work, opts.cross)
