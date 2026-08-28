#!/usr/bin/env python3
"""Contre-épreuve de la porte de release : un préflight rouge ne crée RIEN.

    python3 scripts/contre-epreuve-porte-release.py

Ce que la contre-épreuve établit, et ce qu'elle n'établit pas
─────────────────────────────────────────────────────────────
Le seul essai qui prouverait la chose de bout en bout — pousser un tag dont
le préflight échoue, puis constater qu'aucun brouillon n'existe — demande de
faire tourner `release.yml`. Or un run de `release.yml` PUBLIE. On ne peut
donc pas l'exécuter pour vérifier une garde ; il faut établir la garde
autrement, et le dire.

Ce script établit trois choses, sur le fichier réel, sans le paraphraser :

1. le job qui crée la release (`release` / « Create Release ») ne démarre que
   si `preflight` ET `build` ont réussi — la condition est LUE dans le YAML,
   décomposée en conjonctions, et évaluée sur les 16 combinaisons de
   résultats possibles ;
2. aucune autre partie du workflow ne crée de release ni ne téléverse
   d'actif : la création du brouillon et le dépôt des fichiers vivent
   entièrement dans `release`, et `publish` dépend de `release`. Un job qui
   ne démarre pas ne peut donc laisser ni brouillon ni SHA256SUMS vide ;
3. le contrôle DISCRIMINE : rejoué sur la porte d'avant (`needs: build`,
   `if: always()`), il échoue. Un contrôle qui passerait sur les deux
   versions ne prouverait rien.

Sortie : code 0 si la porte est fermée, 1 sinon.
"""
from __future__ import annotations

import re
import sys
from itertools import product
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RELEASE_YML = REPO_ROOT / ".github" / "workflows" / "release.yml"

# Les quatre résultats qu'un `needs` peut porter côté GitHub Actions.
RESULTS = ("success", "failure", "skipped", "cancelled")

# Ce qui crée une release ou y dépose un fichier. Cherché dans TOUT le
# workflow : la garde ne vaut que si la création est concentrée au même
# endroit.
CREATION_MARKERS = (
    "softprops/action-gh-release",
    "gh release create",
    "gh release upload",
    "gh release edit",
)
# Les seuls jobs autorisés à en porter, et pourquoi.
CREATION_ALLOWED = {
    "release": "crée le brouillon et y dépose les actifs",
    "publish": "retire le brouillon, et dépend de `release`",
}

STATUS_FUNCTIONS = ("always(", "failure(", "cancelled(", "success(")


class Echec(Exception):
    pass


# ─── Lecture du YAML, sans dépendance externe ─────────────────────────


def decouper_les_jobs(texte: str) -> dict[str, list[str]]:
    """Rend {nom_de_job: lignes}. Un job = une clé à deux espaces sous `jobs:`."""
    lignes = texte.splitlines()
    try:
        depart = next(i for i, l in enumerate(lignes) if l.rstrip() == "jobs:")
    except StopIteration as exc:
        raise Echec("aucune section `jobs:` dans release.yml") from exc
    jobs: dict[str, list[str]] = {}
    courant: str | None = None
    for ligne in lignes[depart + 1 :]:
        entete = re.match(r"^  ([A-Za-z_][\w-]*):\s*$", ligne)
        if entete:
            courant = entete.group(1)
            jobs[courant] = []
            continue
        if ligne.strip() and not ligne.startswith("  ") and not ligne.startswith("#"):
            courant = None  # retour au niveau racine : fin de `jobs:`
            continue
        if courant is not None:
            jobs[courant].append(ligne)
    if not jobs:
        raise Echec("aucun job trouvé sous `jobs:`")
    return jobs


def lire_champ(lignes: list[str], champ: str) -> str:
    """Rend la valeur d'une clé de job (`needs:`, `if:`), repliée sur une ligne.

    Gère la forme sur place (`if: expr`) et les blocs `>-` / `|` continués sur
    les lignes suivantes, sans quoi une condition écrite sur trois lignes — la
    nôtre — serait lue comme vide, et le contrôle passerait sur du vide.
    """
    for i, ligne in enumerate(lignes):
        m = re.match(rf"^    {champ}:\s*(.*)$", ligne)
        if not m:
            continue
        valeur = m.group(1).strip()
        if valeur in (">-", ">", "|", "|-", ""):
            morceaux = []
            for suite in lignes[i + 1 :]:
                if suite.strip() == "":
                    continue
                if not suite.startswith("      "):
                    break
                morceaux.append(suite.strip())
            valeur = " ".join(morceaux)
        return valeur.strip()
    return ""


def lire_needs(lignes: list[str]) -> set[str]:
    valeur = lire_champ(lignes, "needs")
    if valeur:
        return {n.strip() for n in valeur.strip("[]").split(",") if n.strip()}
    # Forme liste sur plusieurs lignes.
    for i, ligne in enumerate(lignes):
        if re.match(r"^    needs:\s*$", ligne):
            noms = set()
            for suite in lignes[i + 1 :]:
                m = re.match(r"^      -\s*(\S+)\s*$", suite)
                if not m:
                    break
                noms.add(m.group(1))
            return noms
    return set()


# ─── Évaluation de la condition ───────────────────────────────────────


def conjonctions(expression: str) -> list[str]:
    normalisee = " ".join(expression.split())
    if "||" in normalisee:
        raise Echec(
            f"la condition contient un `||`, hors du gabarit vérifiable : {normalisee!r}"
        )
    return [c.strip() for c in normalisee.split("&&") if c.strip()]


def porte_demarre(expression: str, resultats: dict[str, str]) -> bool:
    """Évalue la condition du job pour un jeu de résultats de `needs`.

    Gabarit accepté, volontairement étroit : une conjonction de comparaisons
    `needs.<job>.result == '<valeur>'`. Tout le reste est refusé plutôt
    qu'interprété — un évaluateur maison qui devinerait la sémantique de
    GitHub serait une preuve fabriquée.
    """
    for conjonction in conjonctions(expression):
        m = re.fullmatch(
            r"needs\.([A-Za-z_][\w-]*)\.result\s*==\s*'([a-z]+)'", conjonction
        )
        if not m:
            raise Echec(f"conjonction hors gabarit : {conjonction!r}")
        job, attendu = m.group(1), m.group(2)
        if job not in resultats:
            raise Echec(f"la condition parle de `{job}`, absent de `needs`")
        if resultats[job] != attendu:
            return False
    return True


# ─── Les trois contrôles ──────────────────────────────────────────────


def controler_la_porte(jobs: dict[str, list[str]]) -> list[str]:
    if "release" not in jobs:
        raise Echec("pas de job `release` dans release.yml")
    lignes = jobs["release"]
    traces = []

    needs = lire_needs(lignes)
    manquants = {"preflight", "build"} - needs
    if manquants:
        raise Echec(
            f"le job `release` ne dépend pas de {sorted(manquants)} (needs = {sorted(needs)})"
        )
    traces.append(f"needs = {sorted(needs)} — contient bien preflight et build")

    condition = lire_champ(lignes, "if")
    if not condition:
        raise Echec("le job `release` n'a aucune condition `if:`")
    for fonction in STATUS_FUNCTIONS:
        if fonction in condition:
            raise Echec(
                f"la condition contient `{fonction})`, qui neutralise le résultat "
                f"des `needs` : {condition!r}"
            )
    traces.append(f"if = {' '.join(condition.split())}")
    traces.append("aucune fonction d'état (always/failure/cancelled) dans la condition")

    # Table de vérité complète sur les résultats de preflight et build.
    ordre = sorted(needs)
    demarrages = []
    for combinaison in product(RESULTS, repeat=len(ordre)):
        resultats = dict(zip(ordre, combinaison))
        if porte_demarre(condition, resultats):
            demarrages.append(resultats)
    attendu = [{job: "success" for job in ordre}]
    if demarrages != attendu:
        raise Echec(
            f"la porte démarre sur {demarrages}, alors qu'elle ne devrait démarrer "
            f"que sur {attendu}"
        )
    traces.append(
        f"{len(RESULTS) ** len(ordre)} combinaisons de résultats évaluées : "
        f"une seule démarre le job, {attendu[0]}"
    )
    for job in ordre:
        rouge = {j: "success" for j in ordre}
        rouge[job] = "failure"
        assert not porte_demarre(condition, rouge)
        saute = dict(rouge, **{job: "skipped"})
        assert not porte_demarre(condition, saute)
    traces.append(
        "préflight en échec → job non démarré ; préflight sauté → job non démarré"
    )
    return traces


def controler_l_unicite_de_la_creation(jobs: dict[str, list[str]]) -> list[str]:
    """Un job qui ne démarre pas ne laisse rien — encore faut-il qu'il soit seul."""
    traces = []
    trouves: dict[str, list[str]] = {}
    for nom, lignes in jobs.items():
        for ligne in lignes:
            nu = ligne.split("#", 1)[0]
            for marqueur in CREATION_MARKERS:
                if marqueur in nu:
                    trouves.setdefault(nom, []).append(marqueur)
    if not trouves:
        raise Echec(
            "aucun marqueur de création de release trouvé — le contrôle ne "
            "regarde pas le bon fichier"
        )
    intrus = set(trouves) - set(CREATION_ALLOWED)
    if intrus:
        raise Echec(
            f"des jobs hors de la porte créent ou modifient la release : {sorted(intrus)}"
        )
    for nom, marqueurs in sorted(trouves.items()):
        traces.append(
            f"`{nom}` ({CREATION_ALLOWED[nom]}) : {', '.join(sorted(set(marqueurs)))}"
        )
    if "publish" in trouves and "release" not in lire_needs(jobs["publish"]):
        raise Echec("`publish` ne dépend pas de `release`")
    traces.append("`publish` dépend de `release` : la bascule en public reste derrière")
    return traces


PORTE_DAVANT = """jobs:
  release:
    name: Create Release
    needs: build
    if: always()
    runs-on: ubuntu-latest
    steps:
      - uses: softprops/action-gh-release@v2
  publish:
    needs: [build, release]
    runs-on: ubuntu-latest
    steps:
      - run: gh release edit "$TAG" --draft=false
"""


def controler_que_le_controle_discrimine() -> list[str]:
    """Contrôle positif : la porte d'AVANT doit faire échouer le contrôle."""
    try:
        controler_la_porte(decouper_les_jobs(PORTE_DAVANT))
    except Echec as e:
        return [f"porte d'avant (`needs: build` + `if: always()`) refusée : {e}"]
    raise Echec(
        "le contrôle accepte la porte d'AVANT : il ne prouve rien sur celle "
        "d'après"
    )


def main() -> int:
    print(f"Contre-épreuve de la porte de release — {RELEASE_YML.relative_to(REPO_ROOT)}")
    print("─" * 72)
    try:
        jobs = decouper_les_jobs(RELEASE_YML.read_text(encoding="utf-8"))
        print(f"  jobs examinés : {len(jobs)} — {', '.join(sorted(jobs))}")
        etapes = [
            ("1. la porte ne s'ouvre que sur préflight + build", controler_la_porte(jobs)),
            (
                "2. rien d'autre ne crée de release ni ne dépose d'actif",
                controler_l_unicite_de_la_creation(jobs),
            ),
            ("3. le contrôle discrimine", controler_que_le_controle_discrimine()),
        ]
    except Echec as e:
        print(f"  [ÉCHEC] {e}")
        return 1
    for titre, traces in etapes:
        print(f"\n  [OK] {titre}")
        for trace in traces:
            print(f"        · {trace}")
    print("\n" + "─" * 72)
    print(
        "  → préflight rouge ⇒ `Create Release` ne démarre pas ⇒ ni brouillon,\n"
        "    ni SHA256SUMS vide. Le run de bout en bout n'est pas rejouable ici :\n"
        "    faire tourner release.yml, c'est publier."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
