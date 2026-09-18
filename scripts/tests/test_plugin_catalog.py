"""Exercise real workflow expansion and reject omitted plugins/commands."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("catalog", ROOT / "scripts/plugin-catalog.py")
catalog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(catalog)


class PluginCatalog(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="tune-catalog-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        shutil.copytree(ROOT / "sdk", self.root / "sdk", ignore=shutil.ignore_patterns("target", "__pycache__"))
        shutil.copytree(ROOT / ".github/workflows", self.root / ".github/workflows")
        shutil.copy2(ROOT / "Cargo.toml", self.root / "Cargo.toml")
        for member in catalog.read_toml(ROOT / "Cargo.toml")["workspace"]["members"]:
            (self.root / member).mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / member / "Cargo.toml", self.root / member / "Cargo.toml")

    def mutate_catalog(self, change):
        path = self.root / "sdk/plugins.json"
        data = json.loads(path.read_text(encoding="utf-8"))
        change(data)
        path.write_text(json.dumps(data), encoding="utf-8")

    def test_current_workflows_match_catalog(self):
        self.assertEqual(catalog.generate(self.root), 19)
        entries = json.loads((self.root / "sdk/plugins.json").read_text())["native"]
        self.assertEqual(next(p for p in entries if p["id"] == "equalizer")["bundled_in"], "tune-core")

    def test_next_plugin_reaches_test_clippy_macos_and_docker_arm64(self):
        # Add a real Cargo member/dependency and ONE catalog entry. No workflow edit.
        crate = self.root / "plugins/tune-fixture"
        crate.mkdir()
        (crate / "Cargo.toml").write_text('[package]\nname="tune-fixture"\nversion="0.1.0"\n', encoding="utf-8")
        path = self.root / "Cargo.toml"
        path.write_text(path.read_text(encoding="utf-8").replace('members = [', 'members = ["plugins/tune-fixture", '), encoding="utf-8")
        path = self.root / "tune-server/Cargo.toml"
        source = path.read_text(encoding="utf-8").replace('[features]', '[features]\ncatalog-fixture = ["dep:tune-fixture"]')
        source += '\n[dependencies.tune-fixture]\npath="../plugins/tune-fixture"\noptional=true\n'
        path.write_text(source, encoding="utf-8")
        self.mutate_catalog(lambda data: data["in_tree"].append({"id": "fixture", "crate": "../plugins/tune-fixture", "feature": "catalog-fixture", "fast_test": False}))
        with self.assertRaisesRegex(ValueError, "stale plugin lists"):
            catalog.generate(self.root)
        catalog.generate(self.root, write=True)
        catalog.generate(self.root)
        for name in catalog.WORKFLOWS:
            lines = (self.root / ".github/workflows" / name).read_text(encoding="utf-8").splitlines()
            for i, line in enumerate(lines):
                if catalog.MARKER not in line:
                    continue
                policy = json.loads(line.split(catalog.MARKER, 1)[1])
                if policy["features"] == "all":
                    self.assertIn("catalog-fixture", lines[i + 1], (name, lines[i + 1]))
                if "packages" in policy:
                    self.assertIn("-p tune-fixture", lines[i + 1])
        docker = (self.root / ".github/workflows/docker.yml").read_text(encoding="utf-8")
        self.assertIn("catalog-fixture", next(line for line in docker.splitlines() if "cross build --release" in line))

    def test_each_generated_line_refuses_manual_plugin_omission(self):
        for name in catalog.WORKFLOWS:
            path = self.root / ".github/workflows" / name
            source = path.read_text(encoding="utf-8")
            lines = source.splitlines(keepends=True)
            for i, line in enumerate(lines):
                if catalog.MARKER not in line:
                    continue
                changed = lines.copy()
                changed[i + 1] = changed[i + 1].replace(",bandcamp", "")
                self.assertNotEqual(changed[i + 1], lines[i + 1])
                path.write_text("".join(changed), encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "stale plugin lists"):
                    catalog.generate(self.root)
                path.write_text(source, encoding="utf-8")

    def test_unregistered_plugin_and_missing_native_plugin_refuse(self):
        self.mutate_catalog(lambda data: data["in_tree"].pop())
        with self.assertRaisesRegex(ValueError, "dependencies"):
            catalog.generate(self.root)
        shutil.copy2(ROOT / "sdk/plugins.json", self.root / "sdk/plugins.json")
        self.mutate_catalog(lambda data: data["native"].pop())
        with self.assertRaisesRegex(ValueError, "native plugin missing"):
            catalog.generate(self.root)

    def test_removing_a_marker_or_the_workflow_check_refuses(self):
        path = self.root / ".github/workflows/docker.yml"
        source = path.read_text(encoding="utf-8")
        path.write_text("\n".join(line for line in source.splitlines() if catalog.MARKER not in line), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "unmanaged"):
            catalog.generate(self.root)
        path.write_text(source.replace("python scripts/plugin-catalog.py --check", "true"), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "missing catalog check"):
            catalog.generate(self.root)

    def test_bundled_equalizer_cannot_become_optional_or_disappear(self):
        path = self.root / "tune-core/Cargo.toml"
        original = path.read_text(encoding="utf-8")
        dependency = 'tune-plugin-equalizer = { path = "../sdk/tune-plugin-equalizer" }'
        self.assertIn(dependency, original)
        for replacement in ("", dependency.replace(" }", ", optional = true }")):
            path.write_text(original.replace(dependency, replacement), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "unconditional bundled dependency: equalizer"):
                catalog.generate(self.root)
        path.write_text(original, encoding="utf-8")

    def test_shipping_manifests_match_the_commercial_offer(self):
        # Independent business oracle: never derive expected access from the
        # manifest under test. The SDK workflow runs this before expensive builds.
        for plugin, entitlement in {"equalizer": "free", "crossfeed": "crossfeed",
                                    "converter": "batch_converter", "declick": "declick"}.items():
            manifest = json.loads((self.root / f"sdk/tune-plugin-{plugin}/manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["entitlement"], entitlement, plugin)
