#!/usr/bin/env python3
"""Single plugin inventory -> explicit, reviewable Cargo workflow commands.

The commands remain materialized for Tune's independent historical CI guards.
Every consumer runs --check before building: a stale or hand-edited expansion
fails closed. --write updates expansions; it never changes a release ref.
Python 3.11+, standard library only. No Cargo, network, or shell evaluation.
"""
import argparse
import json
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ("ci.yml", "release.yml", "docker.yml")
MARKER = "# plugin-catalog: "
NAME = re.compile(r"[a-z][a-z0-9-]*\Z")


def read_toml(path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


def inventory(root):
    sdk = root / "sdk"
    catalog = json.loads((sdk / "plugins.json").read_text(encoding="utf-8"))
    if catalog.get("version") != 1:
        raise ValueError("unsupported plugin catalog version")
    workspace = read_toml(root / "Cargo.toml")["workspace"]
    members = {str((root / p).resolve()): read_toml(root / p / "Cargo.toml")["package"]["name"]
               for p in workspace["members"]}
    server = read_toml(root / "tune-server/Cargo.toml")
    actual = {str((root / "tune-server" / dep["path"]).resolve()): name
              for name, dep in server["dependencies"].items()
              if isinstance(dep, dict) and dep.get("path", "").startswith("../plugins/")}
    declared = set()
    ids = set()
    features = set()
    for plugin in catalog["in_tree"]:
        path = (sdk / plugin["crate"]).resolve()
        name = members.get(str(path))
        feature = plugin["feature"]
        if (not NAME.fullmatch(plugin["id"]) or not NAME.fullmatch(feature)
                or plugin["id"] in ids or feature in features or str(path) in declared
                or type(plugin["fast_test"]) is not bool
                or actual.get(str(path)) != name or name is None
                or "dep:" + name not in server["features"].get(feature, [])):
            raise ValueError(f"invalid in-tree plugin declaration: {plugin}")
        declared.add(str(path)); ids.add(plugin["id"]); features.add(feature)
        plugin["package"] = name
    if declared != set(actual):
        raise ValueError("plugin catalog differs from tune-server's plugin dependencies")
    native_paths = set()
    sdk_members = set(read_toml(sdk / "Cargo.toml")["workspace"]["members"])
    for plugin in catalog["native"]:
        if (not NAME.fullmatch(plugin["id"]) or plugin["id"] in ids
                or plugin["crate"] not in sdk_members or plugin["crate"] in native_paths):
            raise ValueError(f"invalid native plugin declaration: {plugin}")
        crate = sdk / plugin["crate"]
        manifest = json.loads((crate / "manifest.json").read_text(encoding="utf-8"))
        cargo = read_toml(crate / "Cargo.toml")
        if (manifest["id"] != plugin["id"] or "cdylib" not in cargo["lib"]["crate-type"]
                or not {"native", "schemas"}.issubset(cargo["features"])):
            raise ValueError(f"missing native exports/schema contract: {plugin['id']}")
        for file in ("tests/conformance.rs", "examples/schema.rs", "schemas/config.json"):
            if not (crate / file).is_file():
                raise ValueError(f"missing native plugin witness: {crate / file}")
        ids.add(plugin["id"]); native_paths.add(plugin["crate"])
    discovered = {p.parent.name for p in sdk.glob("tune-plugin-*/manifest.json")}
    if native_paths != discovered:
        raise ValueError("native plugin missing from catalog or manifest inventory")
    # Existing independent guards require the extracted HTTP chain contiguous.
    # This is ordering only; membership still comes from Cargo, not another list.
    ordered = [p for p in ("tune-core", "tune-http-types", "tune-smart-http", "tune-stream-http", "tune-streaming-http", "tune-server") if p in members.values()]
    ordered += [p for p in members.values() if p not in ordered]
    return catalog, ordered


def render(source, catalog, members):
    lines = source.splitlines(keepends=True)
    count = 0
    for index, line in enumerate(lines):
        content = line.lstrip()
        if content.startswith("#"):
            continue
        managed = (re.search(r"(?:cargo|cross) (?:test|clippy|build|check) .*tune-server.*--features [a-z]", line)
                   or re.match(r"features: [a-z]", content))
        if managed and (index == 0 or MARKER not in lines[index - 1]):
            raise ValueError("unmanaged plugin feature line: " + content.strip())
    for index, line in enumerate(lines):
        if MARKER not in line:
            continue
        spec = json.loads(line.split(MARKER, 1)[1])
        if set(spec) - {"features", "base", "packages"} or spec["features"] not in {"all", "fast"}:
            raise ValueError("unknown plugin expansion policy")
        plugins = catalog["in_tree"]
        selected = [p["feature"] for p in plugins if spec["features"] == "all" or p["fast_test"]]
        base = spec["base"]
        if any(not NAME.fullmatch(f) for f in base) or set(base) & {p["feature"] for p in plugins}:
            raise ValueError("plugin feature duplicated in platform policy")
        value = ",".join(base + selected)
        command = lines[index + 1]
        if command.lstrip().startswith("features:"):
            command = command[:len(command) - len(command.lstrip())] + "features: " + value + "\n"
        else:
            command, n = re.subn(r"--features [a-z0-9,-]+", "--features " + value, command)
            if n != 1:
                raise ValueError("plugin marker must precede one explicit feature command")
        if "packages" in spec:
            if spec["packages"] == "workspace":
                packages = members
            elif spec["packages"] == "plugins":
                packages = ["tune-server", "tune-plugin-runtime-wasm"] + [p["package"] for p in plugins]
            else:
                raise ValueError("unknown package expansion policy")
            command, n = re.subn(r"(?:-p [a-z0-9-]+ )+", "".join("-p " + p + " " for p in packages), command)
            if n != 1:
                raise ValueError("plugin marker must precede one package list")
        lines[index + 1] = command
        count += 1
    if not count:
        raise ValueError("workflow has no plugin catalog expansions")
    return "".join(lines), count


def generate(root, write=False):
    catalog, members = inventory(root)
    total = 0
    for name in WORKFLOWS:
        path = root / ".github/workflows" / name
        source = path.read_text(encoding="utf-8")
        generated, count = render(source, catalog, members)
        if "python scripts/plugin-catalog.py --check" not in source:
            raise ValueError(f"{name}: missing catalog check before builds")
        if source != generated:
            if not write:
                raise ValueError(f"{name}: stale plugin lists; run python scripts/plugin-catalog.py --write")
            path.write_text(generated, encoding="utf-8")
        total += count
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        print(f"Plugin catalog: {generate(args.root.resolve(), args.write)} workflow lists verified")
    except (ValueError, KeyError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
