"""Behavioral contracts for the public exporter; no Cargo or network."""
import importlib.util
import json
import re
from pathlib import Path
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "public_catalog", Path(__file__).resolve().parents[1] / "plugin-public-catalog.py")
CATALOG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CATALOG)
READ = Path.read_text


class PublicCatalogTests(unittest.TestCase):
    def entries(self):
        return {entry["id"]: entry for entry in CATALOG.export(CATALOG.ROOT, "1.0.0-rc9")["plugins"]}

    def test_uses_real_versions_and_preserves_unknown_compatibility(self):
        entries = self.entries()
        self.assertEqual(entries["bandcamp"]["version"], "1.0.0-rc9")
        for identity in ["party", "playlists-converter"]:
            manifest = json.loads(READ(CATALOG.ROOT / "tune-server/tests/fixtures/plugins" / identity / "manifest.json"))
            self.assertEqual(entries[identity]["version"], manifest["version"])
            self.assertEqual(entries[identity]["min_tune_version"], manifest["min_server_version"])
        self.assertIsNone(entries["bandcamp"]["min_tune_version"])
        self.assertFalse(entries["circle"]["default_enabled"])
        self.assertIn("Réaffectation", entries["channel-remap"]["description"])
        self.assertIn("Dé-ploc", entries["declick"]["description"])
        self.assertNotIn("dj", entries)
        self.assertNotIn("karaoke", entries)

    def test_catalogue_visibility_inherits_the_actual_sdk_default(self):
        def read(path, *args, **kwargs):
            source = READ(path, *args, **kwargs)
            if str(path).endswith("tune-core/src/plugin_sdk.rs"):
                return source.replace("fn catalogued(&self) -> bool {\n        true", "fn catalogued(&self) -> bool {\n        false", 1)
            return source
        with patch.object(Path, "read_text", read):
            entries = self.entries()
            self.assertNotIn("bandcamp", entries)
            self.assertIn("circle", entries)  # Explicit provider override wins.

    def test_activation_inherits_the_actual_sdk_default(self):
        def read(path, *args, **kwargs):
            source = READ(path, *args, **kwargs)
            if str(path).endswith("tune-core/src/plugin_sdk.rs"):
                return source.replace("fn default_enabled(&self) -> bool {\n        true", "fn default_enabled(&self) -> bool {\n        false", 1)
            return source
        with patch.object(Path, "read_text", read):
            self.assertFalse(self.entries()["assistant-ia"]["default_enabled"])

    def test_bundled_wasm_activation_follows_the_loader_default(self):
        for enabled in [True, False]:
            def read(path, *args, **kwargs):
                source = READ(path, *args, **kwargs)
                if str(path).endswith("tune-server/src/plugins_host.rs"):
                    pattern = r'(\.map\(\|v\| v != "false"\)\s*\.unwrap_or\()(?:true|false)(\);)'
                    changed, count = re.subn(pattern, lambda match: match[1] + str(enabled).lower() + match[2], source, count=1)
                    if count != 1:
                        raise AssertionError("WASM activation fixture no longer matches its source")
                    return changed
                return source
            with patch.object(Path, "read_text", read):
                for identity in ["party", "playlists-converter"]:
                    self.assertEqual(self.entries()[identity]["default_enabled"], enabled)

    def test_manifest_update_reaches_export_without_a_second_version_list(self):
        def read(path, *args, **kwargs):
            source = READ(path, *args, **kwargs)
            if str(path).endswith("fixtures/plugins/playlists-converter/manifest.json"):
                manifest = json.loads(source)
                manifest['version'] = '99.1.2'
                return json.dumps(manifest)
            return source
        with patch.object(Path, "read_text", read):
            self.assertEqual(self.entries()["playlists-converter"]["version"], "99.1.2")

    def test_catalogued_false_removes_an_otherwise_public_plugin(self):
        def read(path, *args, **kwargs):
            source = READ(path, *args, **kwargs)
            if str(path).endswith("tune-circle/src/lib.rs"):
                return source.replace("fn catalogued(&self) -> bool {\n        true", "fn catalogued(&self) -> bool {\n        false")
            return source
        with patch.object(Path, "read_text", read):
            self.assertNotIn("circle", self.entries())

    def test_complex_visibility_expression_is_not_mistaken_for_the_default(self):
        def read(path, *args, **kwargs):
            source = READ(path, *args, **kwargs)
            if str(path).endswith("tune-circle/src/lib.rs"):
                return source.replace("fn catalogued(&self) -> bool {\n        true", "fn catalogued(&self) -> bool {\n        if configured() { true } else { false }")
            return source
        with patch.object(Path, "read_text", read):
            with self.assertRaisesRegex(ValueError, "unsupported public metadata method: catalogued"):
                self.entries()

    def test_unsupported_metadata_fails_instead_of_publishing_a_guess(self):
        def read(path, *args, **kwargs):
            source = READ(path, *args, **kwargs)
            if str(path).endswith("tune-circle/src/lib.rs"):
                return source.replace("tune_core::version()", "computed_version()")
            return source
        with patch.object(Path, "read_text", read):
            with self.assertRaisesRegex(ValueError, "unsupported public metadata"):
                self.entries()


if __name__ == "__main__":
    unittest.main()
