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
import wave
import struct
import zipfile
import sys

# Cargo/CLI output is UTF-8 even when Windows redirects Python to a cp1252 pipe.
sys.stdout.reconfigure(encoding="utf-8")
sys.stderr.reconfigure(encoding="utf-8")


def run(*args, expected=0, quiet=False):
    result = subprocess.run([str(arg) for arg in args], text=True, encoding="utf-8", stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
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
        catalog = json.loads((sdk / "plugins.json").read_text(encoding="utf-8"))
        for template in ["dsp", "batch"] + [plugin["id"] for plugin in catalog["native"]]:
            project = root / f"external {template} plugin"
            command = (binary, "new", f"example-{template}", "--template", template, "--sdk-path", sdk, "--output", project)
            run(*command)
            run(binary, "check", project)
            metadata = json.loads(run("cargo", "metadata", "--manifest-path", project / "Cargo.toml", "--no-deps", "--format-version", "1", quiet=True))
            assert Path(metadata["workspace_root"]).resolve() == project.resolve(), "generated project must own its workspace"
            deps = {d["name"] for package in metadata["packages"] for d in package["dependencies"]}
            assert not deps.intersection({"tune-core", "tune-server"}), "plugin must depend only on public SDK crates"
            run(binary, "test", project)
            run("cargo", "build", "--manifest-path", project / "Cargo.toml", "--features", "native")
            if template == "equalizer":
                source=root/"input.wav"
                with wave.open(str(source),"wb") as w:
                    w.setparams((2,2,48000,1024,"NONE","not compressed"))
                    w.writeframes(b"".join(struct.pack("<h", (i%99-49)*200) for i in range(2048)))
                settings=root/"settings.json"
                settings.write_text(json.dumps(dict(enabled=True,listening="headphones",room_size="medium",speaker_placement="free_standing",bass_gain_db=-6,mid_gain_db=0,treble_gain_db=0,bands=[])), encoding="utf-8")
                capture=root/"captured.wav"
                run(binary,"dev",project,"--input",source,"--output",capture,"--settings",settings)
                with wave.open(str(source),"rb") as a, wave.open(str(capture),"rb") as b:
                    assert a.getparams()==b.getparams()
                    assert a.readframes(1024)!=b.readframes(1024), "native CLI must process WAV samples"
                run(binary,"dev",project,"--input",source,"--output",capture,"--settings",settings,expected=1)
                target=next(line.split(": ",1)[1] for line in run("rustc","-vV",quiet=True).splitlines() if line.startswith("host: "))
                package=root/"equalizer.tuneplugin"
                run(binary,"pack",project,"--target",target,"--output",package)
                with zipfile.ZipFile(package) as z:
                    metadata=json.loads(z.read("package.json"))
                    assert metadata["target"]==target and metadata["abi"]==1
                    assert b"tune-plugin-ready" in z.read("ui/index.html")
                    assert metadata["binary"] in z.namelist()

            before = (project / "src/lib.rs").read_bytes()
            run(*command, expected=1)
            assert (project / "src/lib.rs").read_bytes() == before, "scaffolding must not overwrite a project"
            manifest = json.loads((project / "manifest.json").read_text(encoding="utf-8"))
            manifest["sdk"]["minor"] = 999
            (project / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            run(binary, "check", project, expected=1)
        run(binary, "new", "../escape", "--template", "dsp", "--sdk-path", sdk, "--output", root / "escape", expected=1)
        assert not (root / "escape").exists()
    print("External DSP and batch scaffolding verified; native exports, WAV dev capture and packaging verified; hardware and production UI acceptance are separate.")


if __name__ == "__main__":
    main()
