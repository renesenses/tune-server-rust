#!/usr/bin/env python3
"""Regression witnesses for #8112: airplay-daemon on every shipped Linux arch.

No network, no compiler: the workflows are read as text, and the build script
is driven with a mocked `subprocess.run` that writes a fake ELF header.
"""
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("build_airplay_daemon",
                                              ROOT / "scripts/build-airplay-daemon.py")
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)


def elf(machine):
    """Minimal 64-byte ELF64 little-endian header with the given e_machine."""
    header = bytearray(64)
    header[:4] = b"\x7fELF"
    header[4], header[5] = 2, 1
    header[18:20] = struct.pack("<H", machine)
    return bytes(header)


def step(workflow, marker):
    return next((s for s in workflow.split("      - name: ") if marker in s), "")


class AirplayDaemon8112(unittest.TestCase):
    def test_docker_arm64_gets_a_real_daemon(self):
        workflow = (ROOT / ".github/workflows/docker.yml").read_text()
        self.assertNotIn("touch dist/arm64/airplay-daemon", workflow,
                         "Docker arm64 livre encore un airplay-daemon VIDE (#8112)")
        helper = step(workflow, "python scripts/build-airplay-daemon.py")
        for target in ("aarch64-unknown-linux-gnu --cross", "x86_64-unknown-linux-gnu"):
            self.assertIn(target, helper, f"Docker : airplay-daemon absent pour {target}")
        self.assertIn("ARM aarch64", helper, "Docker : l'ELF arm64 n'est pas verifie")

    def test_release_builds_the_daemon_on_cross_targets_and_fails_loudly(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        helper = step(workflow, "python scripts/build-airplay-daemon.py")
        self.assertTrue(helper, "release.yml ne construit plus airplay-daemon")
        self.assertNotRegex(helper.split("run: |")[0], r"\n\s+if:",
                            "airplay-daemon exclut encore des cibles livrees (ARM sautees, #8112)")
        self.assertIn("--cross", helper)
        self.assertIn("exit 1", helper, "un echec d'airplay-daemon ne fait pas echouer le job")
        self.assertNotIn("::warning::", helper,
                         "un echec d'airplay-daemon n'est encore qu'un avertissement (#8112)")
        verify = step(workflow, "Verify airplay-daemon is an aarch64 binary")
        self.assertIn("aarch64-unknown-linux-gnu", verify)
        self.assertIn("ARM aarch64", verify)

    def test_check_binary_reads_the_elf_machine(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "airplay-daemon"
            binary.write_bytes(elf(0xB7))
            build.check_binary(binary, "aarch64-unknown-linux-gnu")
            with self.assertRaisesRegex(RuntimeError, "expected aarch64"):
                binary.write_bytes(elf(0x3E))
                build.check_binary(binary, "aarch64-unknown-linux-gnu")
            with self.assertRaisesRegex(RuntimeError, "no executable"):
                binary.write_bytes(b"")
                build.check_binary(binary, "aarch64-unknown-linux-gnu")

    def test_failed_build_leaves_no_stale_helper(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "apd"
            (output / "bin").mkdir(parents=True)
            (output / "bin" / "airplay-daemon").write_bytes(elf(0xB7))

            def fail(args, **kwargs):
                if args[:2] == ["cross", "build"]:
                    raise build.subprocess.CalledProcessError(101, args)

            with patch.object(build, "fetch", return_value=Path(tmp)), \
                    patch.object(build.subprocess, "run", side_effect=fail):
                with self.assertRaises(build.subprocess.CalledProcessError):
                    build.build("aarch64-unknown-linux-gnu", output, Path(tmp) / "w", "cross")
            self.assertFalse(output.exists(), "un ancien airplay-daemon reste pret a empaqueter")

    def test_cross_uses_tune_config_not_the_fork_one(self):
        seen = {}
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "src"

            def fake(args, **kwargs):
                seen["args"], seen["env"] = args, kwargs.get("env", {})
                out = source / "target/aarch64-unknown-linux-gnu/release"
                out.mkdir(parents=True)
                (out / "airplay-daemon").write_bytes(elf(0xB7))

            with patch.object(build, "fetch", return_value=source), \
                    patch.object(build.subprocess, "run", side_effect=fake), \
                    patch.dict(build.os.environ, {}, clear=False):
                build.os.environ.pop("CARGO_TARGET_DIR", None)
                build.build("aarch64-unknown-linux-gnu", Path(tmp) / "apd", Path(tmp) / "w", "cross")
            self.assertEqual(seen["args"][:2], ["cross", "build"])
            self.assertIn("--locked", seen["args"])
            self.assertTrue(seen["env"]["CROSS_CONFIG"].endswith("packaging/airplay-daemon/Cross.toml"))
            self.assertTrue((Path(tmp) / "apd/bin/airplay-daemon").stat().st_size > 0)


if __name__ == "__main__":
    unittest.main()
