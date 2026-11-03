#!/usr/bin/env python3
"""Inventaire structurel des tests suivis par Git (Python >= 3.11).

Suit les cibles Cargo explicites/automatiques et les modules Rust externes
(`mod`, `#[path]`, `include!` littéral). Les cfg ne sont pas évalués : un
raccordement ne prouve pas une exécution. Les macros génératrices et chemins
calculés demandent une inspection manuelle. Aucun Cargo build n'est lancé.
"""

import argparse
import json
from pathlib import Path
import re
import subprocess
import tomllib


def inventorier(root):
    fichiers = subprocess.check_output(
        ["git", "ls-files", "-z"], cwd=root
    ).decode().split("\0")
    suivis = {root / p for p in fichiers if p}
    resultat = []
    for manifeste in sorted(p for p in suivis if p.name == "Cargo.toml"):
        config = tomllib.loads(manifeste.read_text())
        if "package" not in config:
            continue
        dossier = manifeste.parent
        tests = {p for p in suivis if p.is_relative_to(dossier / "tests")
                 and p.suffix == ".rs"}
        if not tests:
            continue
        cibles = {}
        for cible in config.get("test", []):
            chemin = dossier / cible.get("path", f"tests/{cible['name']}.rs")
            cibles[chemin] = cible["name"]
        if config["package"].get("autotests", True):
            for p in tests:
                if p.parent == dossier / "tests" or (
                    p.name == "main.rs" and p.parent.parent == dossier / "tests"
                ):
                    cibles.setdefault(p, p.stem if p.name != "main.rs" else p.parent.name)
        rattachements = {}

        def visiter(chemin, cible, racine=False):
            chemin = chemin.resolve()
            if chemin not in suivis or cible in rattachements.get(chemin, set()):
                return
            rattachements.setdefault(chemin, set()).add(cible)
            texte = chemin.read_text()
            # Les déclarations de module de ce dépôt sont à la racine des
            # fichiers ; on ne prend pas les exemples commentés pour du code.
            texte = re.sub(r"(?m)^\s*//.*$", "", texte)
            motif = r'(?:#\[path\s*=\s*"([^"]+)"\]\s*)?(?:pub\s+)?mod\s+(\w+)\s*;'
            for lien, nom in re.findall(motif, texte):
                if lien:
                    visiter(chemin.parent / lien, cible)
                else:
                    base = chemin.parent if racine or chemin.name == "mod.rs" else chemin.with_suffix("")
                    visiter(base / f"{nom}.rs", cible)
                    visiter(base / nom / "mod.rs", cible)
            for lien in re.findall(r'include!\s*\(\s*"([^"]+)"\s*\)', texte):
                visiter(chemin.parent / lien, cible)

        for chemin, cible in cibles.items():
            visiter(chemin, cible, racine=True)
        for p in sorted(tests):
            resultat.append({
                "fichier": str(p.relative_to(root)),
                "crate": config["package"]["name"],
                "statut": "cible" if p in cibles else (
                    "module" if p in rattachements else "non_rattache"
                ),
                "cibles": sorted(rattachements.get(p, [])),
            })
    return resultat


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tous", action="store_true", help="inclure les fichiers rattachés")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    inventaire = inventorier(root)
    print(json.dumps([r for r in inventaire if args.tous or r["statut"] == "non_rattache"],
                     ensure_ascii=False, indent=2))
