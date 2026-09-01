#!/usr/bin/env python3
"""Prépare ou vérifie la PR qui ramène un tag publié vers ``main``.

Le script est volontairement conservateur : une branche déjà utilisée n'est
jamais réécrite, une PR n'est jamais fusionnée, et toute divergence inattendue
fait échouer le garde-fou. Il est appelé après le succès du workflow Release,
depuis la branche par défaut où GitHub enregistre les ``workflow_run``.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import unittest
from dataclasses import dataclass
from typing import Any, Sequence


TAG_STABLE = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+$")


class SyncError(RuntimeError):
    pass


def commande(args: Sequence[str], *, capture: bool = True) -> str:
    resultat = subprocess.run(
        list(args),
        check=False,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )
    if resultat.returncode != 0:
        detail = (resultat.stderr or resultat.stdout or "").strip()
        raise SyncError(f"commande en échec ({resultat.returncode}) : {' '.join(args)}\n{detail}")
    return (resultat.stdout or "").strip()


def nom_branche(tag: str) -> str:
    if not TAG_STABLE.fullmatch(tag):
        raise SyncError(f"tag stable attendu, reçu : {tag!r}")
    return f"post-release/{tag}-vers-main"


def verifier_release(release: dict[str, Any]) -> None:
    if release.get("isDraft") or release.get("isPrerelease"):
        raise SyncError("la release est un brouillon ou une préversion")
    if not release.get("publishedAt"):
        raise SyncError("la release n'est pas publiée")
    assets = release.get("assets") or []
    if not assets:
        raise SyncError("la release publiée ne porte aucun asset")
    incomplets = [
        asset.get("name", "<sans nom>")
        for asset in assets
        if int(asset.get("size") or 0) <= 0
        or not str(asset.get("digest") or "").startswith("sha256:")
    ]
    if incomplets:
        raise SyncError(f"assets vides ou sans digest SHA-256 : {incomplets}")


@dataclass(frozen=True)
class PullRequest:
    number: int
    state: str
    head: str
    base: str
    url: str


def choisir_pr(prs: list[dict[str, Any]], branche: str) -> PullRequest | None:
    candidates = [
        PullRequest(
            number=int(pr["number"]),
            state=str(pr["state"]).upper(),
            head=str(pr["headRefName"]),
            base=str(pr["baseRefName"]),
            url=str(pr["url"]),
        )
        for pr in prs
        if pr.get("headRefName") == branche and pr.get("baseRefName") == "main"
    ]
    ouvertes = [pr for pr in candidates if pr.state == "OPEN"]
    if len(ouvertes) > 1:
        raise SyncError(f"plusieurs PR ouvertes pour {branche} : {[pr.number for pr in ouvertes]}")
    if ouvertes:
        return ouvertes[0]
    if len(candidates) > 1:
        raise SyncError(f"plusieurs anciennes PR pour {branche} : {[pr.number for pr in candidates]}")
    return candidates[0] if candidates else None


def est_ancetre(ancien: str, nouveau: str) -> bool:
    resultat = subprocess.run(
        ["git", "merge-base", "--is-ancestor", ancien, nouveau],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    if resultat.returncode not in (0, 1):
        raise SyncError(f"ascendance illisible : {resultat.stderr.strip()}")
    return resultat.returncode == 0


def synchroniser(tag: str, run_sha: str | None, depot: str) -> None:
    branche = nom_branche(tag)

    commande(["git", "fetch", "--no-tags", "origin", "main"], capture=False)
    commande(
        ["git", "fetch", "--no-tags", "origin", f"refs/tags/{tag}:refs/tags/{tag}"],
        capture=False,
    )
    tag_sha = commande(["git", "rev-parse", f"refs/tags/{tag}^{{commit}}"])
    main_sha = commande(["git", "rev-parse", "refs/remotes/origin/main"])

    if run_sha and tag_sha != run_sha:
        raise SyncError(f"le tag {tag} pointe {tag_sha}, le run Release portait {run_sha}")

    release = json.loads(
        commande(
            [
                "gh",
                "release",
                "view",
                tag,
                "--repo",
                depot,
                "--json",
                "isDraft,isPrerelease,publishedAt,assets",
            ]
        )
    )
    verifier_release(release)

    if est_ancetre(tag_sha, main_sha):
        print(f"{tag} ({tag_sha}) est déjà ancêtre de main ({main_sha}) : rien à faire")
        return

    ref_distante = commande(
        ["git", "ls-remote", "--heads", "origin", f"refs/heads/{branche}"]
    )
    if ref_distante:
        branche_sha = ref_distante.split()[0]
        commande(
            [
                "git",
                "fetch",
                "--no-tags",
                "origin",
                f"refs/heads/{branche}:refs/remotes/origin/{branche}",
            ],
            capture=False,
        )
        if not est_ancetre(tag_sha, branche_sha):
            raise SyncError(
                f"{branche} existe à {branche_sha} mais ne contient pas le tag {tag_sha}"
            )
    else:
        commande(
            ["git", "push", "origin", f"{tag_sha}:refs/heads/{branche}"], capture=False
        )
        branche_sha = tag_sha
        print(f"branche {branche} créée sans réécriture à {tag_sha}")

    prs = json.loads(
        commande(
            [
                "gh",
                "pr",
                "list",
                "--repo",
                depot,
                "--state",
                "all",
                "--base",
                "main",
                "--head",
                branche,
                "--limit",
                "100",
                "--json",
                "number,state,headRefName,baseRefName,url",
            ]
        )
    )
    pr = choisir_pr(prs, branche)
    if pr and pr.state == "OPEN":
        print(f"PR #{pr.number} déjà ouverte et vérifiée : {pr.url}")
        return
    if pr and pr.state == "MERGED":
        commande(["git", "fetch", "--no-tags", "origin", "main"], capture=False)
        main_sha = commande(["git", "rev-parse", "refs/remotes/origin/main"])
        if est_ancetre(tag_sha, main_sha):
            print(f"PR #{pr.number} fusionnée ; {tag} est bien dans main")
            return
        raise SyncError(f"PR #{pr.number} marquée fusionnée mais {tag_sha} n'est pas dans main")
    if pr:
        commande(["gh", "pr", "reopen", str(pr.number), "--repo", depot], capture=False)
        print(f"PR #{pr.number} rouverte : {pr.url}")
        return

    corps = f"""Synchronisation post-release de `{tag}` vers `main`.

- tag immuable : `{tag_sha}`
- branche `main` observée : `{main_sha}`
- branche de tête préparée sans réécriture : `{branche}` (`{branche_sha}`)

Cette PR ne doit être fusionnée ni par squash ni par rebase : utiliser un merge commit afin de conserver le tag comme ancêtre identifiable. Aucune fusion automatique n'est armée. En cas de conflit, résoudre sur la branche sans réécrire son histoire, puis laisser la batterie complète aller au bout.

Refs #2770.
"""
    url = commande(
        [
            "gh",
            "pr",
            "create",
            "--repo",
            depot,
            "--base",
            "main",
            "--head",
            branche,
            "--title",
            f"chore(release): synchroniser {tag} vers main",
            "--body",
            corps,
        ]
    )
    print(f"PR créée sans auto-merge : {url}")


class ContreEpreuves(unittest.TestCase):
    def test_nom_de_branche_stable(self) -> None:
        self.assertEqual(nom_branche("v0.9.125"), "post-release/v0.9.125-vers-main")
        for invalide in ["0.9.125", "v0.9.125-rc1", "main", "v0.9.125/x"]:
            with self.assertRaises(SyncError):
                nom_branche(invalide)

    def test_assets_complets(self) -> None:
        verifier_release(
            {
                "isDraft": False,
                "isPrerelease": False,
                "publishedAt": "2026-08-29T14:50:35Z",
                "assets": [{"name": "tune.tar.gz", "size": 42, "digest": "sha256:abcd"}],
            }
        )

    def test_asset_vide_ou_sans_digest_refuse(self) -> None:
        for asset in [
            {"name": "vide", "size": 0, "digest": "sha256:abcd"},
            {"name": "muet", "size": 42, "digest": None},
        ]:
            with self.assertRaises(SyncError):
                verifier_release(
                    {
                        "isDraft": False,
                        "isPrerelease": False,
                        "publishedAt": "2026-08-29T14:50:35Z",
                        "assets": [asset],
                    }
                )

    def test_pr_ouverte_est_reutilisee(self) -> None:
        pr = choisir_pr(
            [
                {
                    "number": 99,
                    "state": "OPEN",
                    "headRefName": "post-release/v0.9.125-vers-main",
                    "baseRefName": "main",
                    "url": "https://example.invalid/99",
                }
            ],
            "post-release/v0.9.125-vers-main",
        )
        self.assertIsNotNone(pr)
        self.assertEqual(pr.number, 99)

    def test_pr_hors_contrat_est_ignoree(self) -> None:
        self.assertIsNone(
            choisir_pr(
                [
                    {
                        "number": 99,
                        "state": "OPEN",
                        "headRefName": "autre",
                        "baseRefName": "release/v0.9",
                        "url": "https://example.invalid/99",
                    }
                ],
                "post-release/v0.9.125-vers-main",
            )
        )


def main() -> int:
    analyseur = argparse.ArgumentParser()
    analyseur.add_argument("--tag")
    analyseur.add_argument("--run-sha")
    analyseur.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    analyseur.add_argument("--self-test", action="store_true")
    args = analyseur.parse_args()

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ContreEpreuves)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if not args.tag or not args.repo:
        analyseur.error("--tag et --repo (ou GITHUB_REPOSITORY) sont requis")
    try:
        synchroniser(args.tag, args.run_sha, args.repo)
    except SyncError as erreur:
        print(f"ERREUR: {erreur}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
