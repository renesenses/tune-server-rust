#!/usr/bin/env python3
"""Témoins de #8466 : fpcalc (Chromaprint) livré à côté de tune-server.

Sans réseau ni compilation : release.yml est lu comme du texte, et l'étape
« Package (unix) » réelle est exécutée sur des fichiers témoins.
"""
import importlib.util
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text()

spec = importlib.util.spec_from_file_location(
    "archive_instructions", ROOT / "scripts/test-archive-instructions.py")
archive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(archive)


def step(marker):
    return next((s for s in WORKFLOW.split("      - name: ") if s.startswith(marker)), "")


class Fpcalc8466(unittest.TestCase):
    def test_fetch_pins_version_and_verifies_every_sum(self):
        fetch = step("Fetch bundled fpcalc (Chromaprint)")
        self.assertTrue(fetch, "release.yml ne télécharge plus fpcalc (#8466)")
        self.assertIn('CHROMAPRINT_VERSION: "1.6.2"', fetch)
        self.assertIn("https://github.com/acoustid/chromaprint/releases/download/v", fetch)
        self.assertIn("sha256sum -c -", fetch, "la somme SHA-256 n'est pas vérifiée")
        self.assertIn("shasum -a 256 -c -", fetch, "macOS ne vérifie pas la somme")
        sums = dict(re.findall(r"arch=(\S+?);.*?sha=([0-9a-f]{64})", fetch))
        self.assertEqual(set(sums), {"linux-x86_64", "linux-arm64", "macos-x86_64",
                                     "macos-arm64", "windows-x86_64"})
        self.assertNotIn("::warning::", fetch, "un fpcalc manquant ne doit pas passer en silence")
        self.assertIn("matrix.target != 'aarch64-unknown-linux-musl'", fetch)

    def test_linux_sums_match_the_docker_image(self):
        docker = (ROOT / "Dockerfile.dist").read_text()
        fetch = step("Fetch bundled fpcalc (Chromaprint)")
        self.assertIn("ARG CHROMAPRINT_VERSION=1.6.2", docker,
                      "Docker et les archives doivent livrer la même version")
        sums = dict(re.findall(r"arch=(\S+?);.*?sha=([0-9a-f]{64})", fetch))
        self.assertIn(f"FPCALC_SHA256_AMD64={sums['linux-x86_64']}", docker)
        self.assertIn(f"FPCALC_SHA256_ARM64={sums['linux-arm64']}", docker)

    def test_windows_zip_installer_and_signature_carry_fpcalc(self):
        self.assertIn('$targets += "$env:RUNNER_TEMP\\fpcalc\\fpcalc.exe"',
                      step("Sign Windows binaries"), "fpcalc.exe non signé")
        zip_step = step("Package (windows zip)")
        self.assertIn('copy "$env:RUNNER_TEMP\\fpcalc\\fpcalc.exe" package\\', zip_step)
        self.assertIn("fpcalc.exe }", zip_step, "zip Windows sans fpcalc.exe")
        nsis = step("Create installer (windows)")
        self.assertIn("$fpcalcFile = 'File \"target\\${{ matrix.target }}\\release\\fpcalc.exe\"'", nsis)
        self.assertIn("            $fpcalcFile\n", nsis, "l'installeur NSIS ne livre pas fpcalc.exe")

    def test_macos_signs_fpcalc_in_tarball_and_dmg(self):
        self.assertIn('sign_horodate package/fpcalc fr.mozaiklabs.fpcalc ""', step("Package (unix)"))
        dmg = step("Create DMG (macOS)")
        self.assertIn('cp "$RUNNER_TEMP/fpcalc/fpcalc" "$APP/Contents/Resources/"', dmg)
        self.assertIn('--identifier fr.mozaiklabs.fpcalc --sign "$IDENTITY" "$APP/Contents/Resources/fpcalc"', dmg)

    def test_unix_tarball_really_contains_fpcalc(self):
        script = archive.packaging_step()
        for key, value in {"${{ matrix.target }}": "aarch64-apple-darwin",
                           "${{ env.ARTIFACT }}": "temoin", "${{ matrix.ext }}": "tar.gz"}.items():
            script = script.replace(key, value)
        with tempfile.TemporaryDirectory(prefix="fpcalc-8466-") as tmp:
            root = Path(tmp)
            for name, data in {
                "target/aarch64-apple-darwin/release/tune-server": b"server",
                "web/index.html": b"web",
                "tune-server/tests/fixtures/plugins/party/main.wasm": b"p",
                "tune-server/tests/fixtures/plugins/party/manifest.json": b"{}",
                "tune-server/tests/fixtures/plugins/playlists-converter/main.wasm": b"c",
                "tune-server/tests/fixtures/plugins/playlists-converter/manifest.json": b"{}",
                "runner/fpcalc/fpcalc": b"fpcalc officiel",
            }.items():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_bytes(data)
            env = os.environ | {"RUNNER_OS": "macOS", "RUNNER_TEMP": str(root / "runner"),
                                "GITHUB_WORKSPACE": str(root), "APPLE_TEAM_ID": ""}
            subprocess.run(["bash", "-eu", "-o", "pipefail", "-c", script],
                           cwd=root, env=env, check=True, capture_output=True)
            with tarfile.open(root / "temoin.tar.gz") as tar:
                member = tar.getmember("fpcalc")
                self.assertEqual(tar.extractfile(member).read(), b"fpcalc officiel")
                self.assertEqual(member.mode & 0o777, 0o755, "fpcalc non exécutable dans l'archive")


if __name__ == "__main__":
    unittest.main(verbosity=2)
