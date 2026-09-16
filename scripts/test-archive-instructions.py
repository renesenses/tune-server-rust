#!/usr/bin/env python3
"""Execute the real Unix packaging step on offline fixtures (#4233).

No Rust build, package installation, signing, service start or network access.
Only Python's standard library and bash/tar are needed. Run from any directory.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


def packaging_step():
    lines = (ROOT / ".github/workflows/release.yml").read_text().splitlines()
    start = lines.index("      - name: Package (unix)")
    run = lines.index("        run: |", start) + 1
    body = []
    for line in lines[run:]:
        if line and not line.startswith("          "):
            break
        body.append(line[10:] if line else "")
    result = "\n".join(body)
    if not result.strip():
        raise AssertionError("Unix packaging step is empty")
    return result


class ArchiveInstructions(unittest.TestCase):
    def package(self, target, runner, extras):
        with tempfile.TemporaryDirectory(prefix="tune-4233-") as tmp:
            root = Path(tmp)
            inputs = {
                f"target/{target}/release/tune-server": b"server fixture\n",
                "web/index.html": b"web fixture\n",
                "tune-server/tests/fixtures/plugins/party/main.wasm": b"plugin fixture\n",
                "tune-server/tests/fixtures/plugins/party/manifest.json": b"{}\n",
            }
            expected = {
                "tune-server": inputs[f"target/{target}/release/tune-server"],
                "web/index.html": inputs["web/index.html"],
                "plugins/party/main.wasm": inputs["tune-server/tests/fixtures/plugins/party/main.wasm"],
                "plugins/party/manifest.json": b"{}\n",
            }
            if extras:
                inputs.update({
                    "runner/apd/bin/airplay-daemon": b"daemon fixture\n",
                    "runner/ffbundle/ffmpeg": b"ffmpeg fixture\n",
                    "runner/ffbundle/FFMPEG-LICENSE.txt": b"license fixture\n",
                })
                expected.update({
                    "airplay-daemon": b"daemon fixture\n",
                    "ffmpeg": b"ffmpeg fixture\n",
                    "FFMPEG-LICENSE.txt": b"license fixture\n",
                })
            for name, data in inputs.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
            (root / "packaging/linux").mkdir(parents=True)
            shutil.copyfile(ROOT / "packaging/linux/README.txt",
                            root / "packaging/linux/README.txt")
            script = packaging_step()
            for key, value in {
                "${{ matrix.target }}": target,
                "${{ env.ARTIFACT }}": "fixture",
                "${{ matrix.ext }}": "tar.gz",
            }.items():
                script = script.replace(key, value)
            self.assertNotIn("${{", script, "unresolved packaging expression")
            env = os.environ | {
                "RUNNER_OS": runner,
                "RUNNER_TEMP": str(root / "runner"),
                "GITHUB_WORKSPACE": str(root),
                "APPLE_TEAM_ID": "",
            }
            subprocess.run(["bash", "-eu", "-o", "pipefail", "-c", script],
                           cwd=root, env=env, check=True, capture_output=True)
            with tarfile.open(root / "fixture.tar.gz") as archive:
                files = {
                    member.name: archive.extractfile(member).read()
                    for member in archive.getmembers() if member.isfile()
                }
            if runner == "Linux":
                self.assertIn("README.txt", files,
                              "Linux tarball omits installation instructions (#4233)")
                expected["README.txt"] = (ROOT / "packaging/linux/README.txt").read_bytes()
            else:
                self.assertNotIn("README.txt", files,
                                 "Linux instructions must not enter the macOS tarball")
            self.assertEqual(files, expected, "bundled files changed or instructions differ")

    def test_linux_archives_include_instructions(self):
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
                       "aarch64-unknown-linux-musl"):
            for extras in (False, True):
                with self.subTest(target=target, extras=extras):
                    self.package(target, "Linux", extras)

    def test_macos_archive_contents_remain_unchanged(self):
        for target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
            for extras in (False, True):
                with self.subTest(target=target, extras=extras):
                    self.package(target, "macOS", extras)


if __name__ == "__main__":
    unittest.main(verbosity=2)
