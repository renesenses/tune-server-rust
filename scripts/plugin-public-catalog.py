#!/usr/bin/env python3
"""Export public metadata from the SDK inventory and actual plugin declarations.

No plugin is instantiated and no device or network is accessed. Unsupported Rust
metadata expressions fail closed, rather than silently publishing stale values.
"""
import argparse
import json
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]
# This public allowlist is deliberately narrower than the build inventory.
PUBLIC = {
    "bandcamp", "concerts", "pont-roon", "cd", "circle", "entree-audio",
    "assistant-ia", "equalizer", "crossfeed", "converter", "declick",
    "channel-remap", "party", "playlists-converter", "mon-theme",
}


def method(source, name, default=None):
    match = re.search(r"\bfn " + name + r"\(&self\)[^{]*\{([^{}]*)\}", source)
    if match is None:
        if re.search(r"\bfn " + name + r"\(&self\)", source):
            raise ValueError("unsupported public metadata method: " + name)
        return default
    return re.sub(r"//[^\n]*", "", match[1]).strip()


def literal(expression):
    if expression is None:
        return None
    try:
        value = json.loads(expression)
    except (ValueError, TypeError) as error:
        raise ValueError("unsupported public metadata expression: " + str(expression)) from error
    if not isinstance(value, str):
        raise ValueError("public metadata must be a string")
    return value


def boolean(expression):
    if expression not in {"true", "false"}:
        raise ValueError("unsupported public metadata boolean: " + str(expression))
    return expression == "true"


def package_version(root, crate):
    cargo = tomllib.loads((crate / "Cargo.toml").read_text())
    version = cargo["package"]["version"]
    if isinstance(version, dict) and version.get("workspace") is True:
        version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    return version


def export(root, tune_version):
    inventory = json.loads((root / "sdk/plugins.json").read_text())
    if inventory.get("version") != 1:
        raise ValueError("unsupported SDK inventory")
    trait = (root / "tune-core/src/plugin_sdk.rs").read_text()
    defaults = {name: method(trait, name) for name in ("catalogued", "default_enabled")}
    for expression in defaults.values():
        boolean(expression)
    entries = []
    for plugin in inventory["in_tree"]:
        if plugin["id"] not in PUBLIC:
            continue
        crate = (root / "sdk" / plugin["crate"]).resolve()
        source = (crate / "src/lib.rs").read_text()
        if not boolean(method(source, "catalogued", defaults["catalogued"])):
            continue
        identity = literal(method(source, "name"))
        if identity != plugin["id"]:
            raise ValueError("registry identity differs from SDK inventory")
        version_expression = method(source, "version")
        if version_expression == "tune_core::version()":
            version = tune_version
        elif version_expression == 'env!("CARGO_PKG_VERSION")':
            version = package_version(root, crate)
        else:
            version = literal(version_expression)
        entries.append({
            "id": identity,
            "display_name": literal(method(source, "display_name")) or identity,
            "description": literal(method(source, "description")),
            "version": version,
            "min_tune_version": None,
            "kind": "in_tree",
            "default_enabled": boolean(method(source, "default_enabled", defaults["default_enabled"])),
        })
    bridge = (root / "tune-server/src/premium_audio_plugins.rs").read_text()
    native_version = literal(method(bridge, "version"))
    native_enabled = boolean(method(bridge, "default_enabled"))
    description_match = re.search(
        r"fn description\(&self\).*?\n    fn default_enabled", bridge, re.S)
    if description_match is None:
        raise ValueError("missing native registry descriptions")
    native_descriptions = dict(re.findall(
        r'("(?:[^"\\]|\\.)*"|_)\s*=>\s*(?:\{\s*)?("(?:[^"\\]|\\.)*")',
        description_match[0]))
    for plugin in inventory["native"]:
        if plugin["id"] not in PUBLIC:
            continue
        manifest = json.loads((root / "sdk" / plugin["crate"] / "manifest.json").read_text())
        if manifest["id"] != plugin["id"]:
            raise ValueError("native identity differs from SDK inventory")
        description = native_descriptions.get(json.dumps(plugin["id"]))
        if plugin["id"] == "declick":
            description = native_descriptions.get("_")
        if description is None:
            raise ValueError("missing native registry description: " + plugin["id"])
        entries.append({
            "id": plugin["id"], "display_name": plugin["id"],
            "description": literal(description), "version": native_version,
            "min_tune_version": None, "kind": "native",
            "default_enabled": native_enabled,
            "entitlement": manifest["entitlement"],
        })
    host = (root / "tune-server/src/plugins_host.rs").read_text()
    activation = re.search(
        r'let enabled\s*=\s*settings\s*\.get\(&format!\("plugin_\{id\}_enabled"\)\)\s*'
        r'\.ok\(\)\s*\.flatten\(\)\s*\.map\(\|v\| v != "false"\)\s*\.unwrap_or\((true|false)\);',
        host, re.S)
    if activation is None:
        raise ValueError("unsupported bundled WASM activation default")
    wasm_enabled = boolean(activation[1])
    for identity in ("party", "playlists-converter"):
        manifest = json.loads((root / "tune-server/tests/fixtures/plugins" / identity / "manifest.json").read_text())
        if manifest["id"] != identity:
            raise ValueError("WASM identity differs from bundled manifest")
        entries.append({
            "id": identity, "display_name": manifest["name"],
            "description": manifest["description"], "version": manifest["version"],
            "min_tune_version": manifest["min_server_version"], "kind": "wasm",
            "default_enabled": wasm_enabled, "premium": manifest.get("premium", False),
        })
    identities = [entry["id"] for entry in entries]
    if len(identities) != len(set(identities)):
        raise ValueError("duplicate public plugin identity")
    return {"schema_version": 1, "tune_version": tune_version,
            "plugins": sorted(entries, key=lambda entry: entry["id"])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tune-version", required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", args.tune_version):
        parser.error("an explicit Tune build version is required")
    content = json.dumps(export(ROOT, args.tune_version), ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.write_text(content)
    else:
        print(content, end="")


if __name__ == "__main__":
    main()
