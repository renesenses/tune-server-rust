#!/usr/bin/env python3
"""Targeted build/packaging regression witnesses for #6872 (no audio/network)."""
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import textwrap
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("build_librespot", ROOT / "scripts/build-librespot.py")
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


class Librespot6872(unittest.TestCase):
    def test_both_docker_architectures_build_a_real_helper(self):
        workflow = (ROOT / ".github/workflows/docker.yml").read_text()
        steps = workflow.split("      - name: ")
        helper = next((s for s in steps if "scripts/build-librespot.py" in s), "")
        for target in ("aarch64-unknown-linux-gnu --cross", "x86_64-unknown-linux-gnu"):
            self.assertIn(target, helper, f"Docker: librespot absent pour {target}")
        self.assertNotIn("touch dist/arm64/librespot", workflow,
                         "Docker ARM64 livre encore un librespot vide")
        self.assertIn("LIBRESPOT-*.txt", (ROOT / "Dockerfile.dist").read_text(),
                      "Docker perd les mentions des dépendances")

    def test_release_build_covers_every_matrix_target(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        step = next((s for s in workflow.split("      - name: ")
                     if "scripts/build-librespot.py" in s), "")
        self.assertTrue(step, "librespot absent des builds macOS, ARM64 et Windows")
        self.assertNotRegex(step.split("run: |")[0], r"\n\s+if:",
                            "librespot exclut encore des plateformes livrées")
        self.assertIn('--target ${{ matrix.target }}', step)
        self.assertIn("--cross", step)

    def test_windows_both_packages_and_signing_include_helper(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn('$targets += "$env:RUNNER_TEMP\\lrs\\bin\\librespot.exe"', workflow,
                      "librespot.exe échappe à la signature Windows existante")
        zip_step = workflow.split("- name: Package (windows zip)")[1].split("      - name: ")[0]
        self.assertRegex(zip_step, r"7z a .* librespot.exe LIBRESPOT-LICENSE.txt LIBRESPOT-THIRD-PARTY.txt",
                         "zip Windows sans librespot ou sans mentions")
        for filename in ("librespot.exe", "LIBRESPOT-LICENSE.txt", "LIBRESPOT-THIRD-PARTY.txt"):
            self.assertIn(f'File "package\\{filename}"', workflow,
                          f"NSIS ne livre pas {filename}")

    def test_mac_reuses_timestamped_helper_inside_app(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn('sign_horodate package/librespot fr.mozaiklabs.librespot ""', workflow,
                      "librespot macOS manque de signature horodatée")
        self.assertIn('codesign --verify --strict --verbose=2 package/librespot', workflow)
        self.assertIn('cp package/librespot "$APP/Contents/Resources/"', workflow,
                      "DMG sans librespot signé")
        self.assertIn('cp package/LIBRESPOT-*.txt "$APP/Contents/Resources/"', workflow,
                      "DMG sans mentions")

    def test_unix_tarball_really_contains_binary_and_both_notices(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        stage = workflow.split('          LIBRESPOT=""', 1)[1].split('          # ffmpeg minimal', 1)[0]
        command = next(line.strip() for line in workflow.splitlines()
                       if "cd package && tar czf" in line)
        command = command.replace("${{ env.ARTIFACT }}", "witness").replace("${{ matrix.ext }}", "tar.gz")
        with tempfile.TemporaryDirectory(prefix="archive-librespot-6872-") as tmp:
            base = Path(tmp)
            lrs = base / "lrs"
            (lrs / "bin").mkdir(parents=True)
            (lrs / "bin/librespot").write_bytes(b"helper")
            for name in ("LIBRESPOT-LICENSE.txt", "LIBRESPOT-THIRD-PARTY.txt"):
                (lrs / name).write_text(name)
            package = base / "package"
            (package / "web").mkdir(parents=True)
            (package / "plugins").mkdir()
            (package / "tune-server").write_bytes(b"server")
            env = dict(os.environ, RUNNER_TEMP=str(base))
            subprocess.run(["bash", "-eu", "-c", 'LIBRESPOT=""\n' + textwrap.dedent(stage) +
                            "\nDAEMON= FFMPEG= INSTRUCTIONS=\n" + command],
                           cwd=base, env=env, check=True)
            with tarfile.open(base / "witness.tar.gz") as archive:
                for name in ("librespot", "LIBRESPOT-LICENSE.txt", "LIBRESPOT-THIRD-PARTY.txt"):
                    self.assertIn(name, archive.getnames(), f"archive Unix sans {name}")
                    self.assertTrue(archive.extractfile(name).read())

    def fake_build(self, target, *, fail=None, empty=False, notices=True):
        with tempfile.TemporaryDirectory(prefix="librespot-6872-") as tmp:
            root = Path(tmp)
            blob = io.BytesIO()
            with tarfile.open(fileobj=blob, mode="w:gz") as archive:
                for name, data in (("Cargo.toml", b"[package]\n"), ("LICENSE", b"MIT witness")):
                    item = tarfile.TarInfo(f"librespot-{build.VERSION}/{name}")
                    item.size = len(data)
                    archive.addfile(item, io.BytesIO(data))
            data = blob.getvalue()
            output = root / "output"
            output.mkdir()
            (output / "stale").write_text("old helper")
            commands = []

            def run(cmd, **kwargs):
                commands.append(cmd)
                if fail == len(commands):
                    raise subprocess.CalledProcessError(1, cmd)
                if "build" in cmd:
                    self.assertIn("--locked", cmd)
                    self.assertIn("--no-default-features", cmd)
                    self.assertEqual(cmd[cmd.index("--jobs") + 1], "4")
                    self.assertEqual(cmd[cmd.index("--features") + 1], build.FEATURES)
                    if "windows" in target:
                        self.assertIn("+crt-static", kwargs["env"]["RUSTFLAGS"])
                    binary = root / "target" / target / "release" / (
                        "librespot.exe" if "windows" in target else "librespot")
                    binary.parent.mkdir(parents=True)
                    binary.write_bytes(b"" if empty else b"compiled helper")
                elif notices:
                    self.assertIn("--locked", cmd)
                    self.assertIn("--fail", cmd)
                    Path(cmd[cmd.index("--output-file") + 1]).write_text("Dependency notice")

            with patch.object(build.urllib.request, "urlopen", return_value=io.BytesIO(data)), \
                 patch.object(build, "SHA256", hashlib.sha256(data).hexdigest()), \
                 patch.object(build.subprocess, "run", side_effect=run), \
                 patch.dict(os.environ, {"CARGO_TARGET_DIR": str(root / "target")}):
                if fail or empty or not notices:
                    with self.assertRaises((RuntimeError, subprocess.CalledProcessError)):
                        build.build(target, output, root / "work", cross="linux" in target)
                    self.assertFalse(output.exists(), "échec laissant un ancien helper prêt à livrer")
                    return
                build.build(target, output, root / "work", cross="linux" in target)
            name = "librespot.exe" if "windows" in target else "librespot"
            self.assertEqual((output / "bin" / name).read_bytes(), b"compiled helper")
            self.assertEqual((output / "LIBRESPOT-LICENSE.txt").read_text(), "MIT witness")
            self.assertEqual((output / "LIBRESPOT-THIRD-PARTY.txt").read_text(), "Dependency notice")
            self.assertFalse((output / "stale").exists())
            self.assertEqual(len(commands), 2, "ne jamais exécuter le binaire étranger sur l'hôte")

    def test_build_stages_binary_and_notices_for_six_targets(self):
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
                       "aarch64-unknown-linux-musl", "x86_64-apple-darwin",
                       "aarch64-apple-darwin", "x86_64-pc-windows-msvc"):
            with self.subTest(target=target):
                self.fake_build(target)

    def test_failure_does_not_ship_stale_empty_or_unlicensed_binary(self):
        for opts in ({"fail": 1}, {"fail": 2}, {"empty": True}, {"notices": False}):
            with self.subTest(opts=opts):
                self.fake_build("aarch64-unknown-linux-gnu", **opts)


if __name__ == "__main__":
    unittest.main(verbosity=2)
