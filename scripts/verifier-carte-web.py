#!/usr/bin/env python3
"""La carte `docs/contrat-web.json` décrit-elle encore le client web publié ?

CE QUI MANQUAIT, ET POURQUOI C'EST LE PIRE DES DÉFAUTS
    `tune-server/tests/web_response_contracts.rs` CHARGE cette carte pour juger
    les réponses du serveur. Personne ne jugeait la carte elle-même :
    `scripts/web-contract-map.py` n'était exécuté par aucun poste de la CI.

    Résultat mesuré le 11/09/2026 : la carte n'avait pas bougé depuis le 31/08
    pendant que `api.ts` recevait 71 commits. Elle citait encore
    `/playlists/transfer`, deux routes `media-servers` et `/network/shares/{}`,
    que le client n'appelle plus. On validait contre un document figé en
    croyant vérifier un contrat — un instrument qui ment, ce qui est pire que
    pas d'instrument du tout : il donne l'illusion de la preuve.

LES TROIS CLASSES D'ÉCART, ET CE QU'ELLES COÛTENT
    PÉRIMÉE   La carte cite une route que le client n'appelle plus du tout.
              Le banc d'essai fait alors respecter au serveur un contrat que
              plus personne ne lit : il bloque des changements légitimes et
              fige du code mort. C'est le cas de `/library/tracks/{}/lyrics`,
              où la carte recopiait un type sans appelant (#3002).
              → BLOQUANTE.

    MANQUANTE Le client appelle une route que la carte ignore. Le banc d'essai
              ne la joue jamais, donc aucune dérive de champs n'y est visible.
              C'est exactement par ce trou qu'est passé #3002.
              → SIGNALÉE, PAS BLOQUANTE (voir « paliers » plus bas).

    CHAMPS    Route connue des deux côtés, exigences différentes.
              → SIGNALÉE, PAS BLOQUANTE.

POURQUOI UNE SEULE CLASSE BLOQUE POUR COMMENCER
    Au 11/09/2026, la comparaison sort 8 routes périmées, 26 manquantes et 41
    contrats de champs divergents. Rendre les trois classes bloquantes d'emblée
    aurait produit un rouge permanent, et un contrôle qui rougit toujours finit
    par être contourné — ce dépôt en a déjà fait deux fois les frais.

    La classe PÉRIMÉE retombe à zéro dès que la carte est régénérée, et n'y
    remonte que lorsqu'un écran cesse d'appeler une route. Elle bloque donc
    rarement, et quand elle bloque elle a raison.

    Palier suivant, à armer quand la carte sera régénérée à chaque bump web :
    passer MANQUANTE en bloquante avec `--exiger-complet`.

USAGE
    scripts/verifier-carte-web.py --web ../tune-web-client
    scripts/verifier-carte-web.py --web ../tune-web-client --exiger-complet
    scripts/verifier-carte-web.py --self-test
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
from pathlib import Path

RACINE = Path(__file__).resolve().parent.parent
CARTE_COMMITEE = RACINE / "docs" / "contrat-web.json"


def _charger_cartographe():
    """`web-contract-map.py` porte un tiret : pas d'import direct possible.

    On le charge comme module plutôt que de recopier son extraction ici. Deux
    extracteurs divergeraient, et c'est précisément le genre de divergence
    silencieuse que ce contrôle existe pour attraper.
    """
    chemin = Path(__file__).resolve().parent / "web-contract-map.py"
    spec = importlib.util.spec_from_file_location("web_contract_map", chemin)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"{chemin} illisible")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def cle(entree: dict) -> tuple[str, str, str]:
    return (entree["methode"], entree["route"], entree["type"])


def comparer(commitee: dict, regeneree: dict) -> dict[str, list]:
    """Les trois classes d'écart entre la carte du dépôt et celle du client.

    La classe PÉRIMÉE se juge sur la ROUTE, pas sur la clef complète : un type
    renommé côté web (`RechercheRadios` → `StreamingSearchResult`) déplace
    l'entrée sans que la route cesse d'être appelée. Confondre les deux ferait
    rougir un simple renommage, et ce contrôle serait ignoré en une semaine.
    """
    anciennes = {cle(e): e for e in commitee["routes"]}
    nouvelles = {cle(e): e for e in regeneree["routes"]}
    routes_web = {(e["methode"], e["route"]) for e in regeneree["routes"]}
    routes_carte = {(e["methode"], e["route"]) for e in commitee["routes"]}

    perimees = sorted(routes_carte - routes_web)
    manquantes = sorted(routes_web - routes_carte)
    champs = []
    for k in sorted(anciennes.keys() & nouvelles.keys()):
        avant, apres = anciennes[k], nouvelles[k]
        if (
            avant["champs_obligatoires"] != apres["champs_obligatoires"]
            or avant["liste"] != apres["liste"]
        ):
            champs.append(
                (
                    k,
                    sorted(set(avant["champs_obligatoires"]) - set(apres["champs_obligatoires"])),
                    sorted(set(apres["champs_obligatoires"]) - set(avant["champs_obligatoires"])),
                )
            )
    return {"perimees": perimees, "manquantes": manquantes, "champs": champs}


def rapporter(ecarts: dict[str, list], exiger_complet: bool) -> int:
    perimees, manquantes, champs = (
        ecarts["perimees"],
        ecarts["manquantes"],
        ecarts["champs"],
    )
    print(
        f"carte vs client web : {len(perimees)} périmée(s), "
        f"{len(manquantes)} manquante(s), {len(champs)} contrat(s) de champs divergent(s)"
    )

    for methode, route in manquantes:
        print(f"::warning::carte incomplète : {methode} {route} est appelée par le web, "
              f"absente de docs/contrat-web.json — aucune dérive n'y est visible")
    for (methode, route, type_web), retires, ajoutes in champs:
        print(f"::warning::contrat divergent : {methode} {route} ({type_web}) "
              f"— la carte exige en trop {retires or '[]'}, ignore {ajoutes or '[]'}")

    if perimees:
        print()
        print(f"✗ {len(perimees)} route(s) citée(s) par docs/contrat-web.json que le "
              f"client web n'appelle plus :")
        for methode, route in perimees:
            print(f"    {methode} {route}")
        print()
        print("Le banc d'essai `web_response_contracts` fait respecter au serveur un")
        print("contrat que plus aucun écran ne lit. Régénérer la carte :")
        print("    scripts/web-contract-map.py --web <tune-web-client> -o docs/contrat-web.json")
        return 1

    print("✓ aucune route périmée dans docs/contrat-web.json")
    if exiger_complet and manquantes:
        print()
        print(f"✗ --exiger-complet : {len(manquantes)} route(s) appelée(s) par le web "
              f"manquent à la carte.")
        return 1
    return 0


def self_test() -> int:
    """Le contrôle attrape-t-il chaque classe, et se tait-il quand tout va bien ?"""
    echecs = []

    def entree(methode, route, type_web, obligatoires, liste=False):
        return {
            "route": route,
            "methode": methode,
            "type": type_web,
            "liste": liste,
            "champs_obligatoires": obligatoires,
            "champs_optionnels": [],
        }

    # Les routes témoins sont ASSEMBLÉES : une aiguille écrite en clair serait
    # extraite par le cartographe le jour où ce fichier tomberait dans son
    # champ de lecture, et le contrôle se trouverait lui-même.
    morte = "/" + "/".join(["playlists", "transfer"])
    neuve = "/" + "/".join(["ext", "concerts", "upcoming"])
    stable = "/" + "zones"

    commitee = {
        "routes": [
            entree("GET", stable, "Zone", ["id", "name"], liste=True),
            entree("POST", morte, "PlaylistTransferResponse", ["moved"]),
        ]
    }
    regeneree = {
        "routes": [
            entree("GET", stable, "Zone", ["id", "name", "output_type"], liste=True),
            entree("GET", neuve, "ConcertsAVenir", ["items"]),
        ]
    }

    ecarts = comparer(commitee, regeneree)
    if ("POST", morte) not in ecarts["perimees"]:
        echecs.append("une route que le web n'appelle plus n'est PAS signalée périmée")
    if ("GET", neuve) not in ecarts["manquantes"]:
        echecs.append("une route appelée par le web et absente de la carte n'est pas signalée")
    if not any(k[1] == stable for k, _, _ in ecarts["champs"]):
        echecs.append("un champ obligatoire ajouté par le web ne produit aucun écart")
    if rapporter(ecarts, exiger_complet=False) != 1:
        echecs.append("une route périmée ne fait PAS échouer le contrôle")

    # Contre-épreuve : cartes identiques, aucun écart, aucun rouge.
    identique = comparer(commitee, commitee)
    if any(identique.values()):
        echecs.append(f"deux cartes identiques produisent des écarts : {identique}")
    if rapporter(identique, exiger_complet=True) != 0:
        echecs.append("deux cartes identiques font échouer le contrôle")

    # Un TYPE renommé sans changement de route ne doit pas compter comme périmé :
    # sinon le moindre renommage TypeScript rendrait ce contrôle rouge, donc
    # ignoré.
    renomme = comparer(
        {"routes": [entree("GET", stable, "AncienNom", ["id"])]},
        {"routes": [entree("GET", stable, "NouveauNom", ["id"])]},
    )
    if renomme["perimees"] or renomme["manquantes"]:
        echecs.append(f"un simple renommage de type est compté comme un écart : {renomme}")

    # `--exiger-complet` doit bien durcir la classe MANQUANTE, sinon le palier
    # suivant n'existerait que sur le papier.
    seulement_manquante = comparer(
        {"routes": [entree("GET", stable, "Zone", ["id"])]},
        {"routes": [entree("GET", stable, "Zone", ["id"]), entree("GET", neuve, "X", ["items"])]},
    )
    if rapporter(seulement_manquante, exiger_complet=False) != 0:
        echecs.append("une route manquante bloque alors qu'elle ne devrait que prévenir")
    if rapporter(seulement_manquante, exiger_complet=True) != 1:
        echecs.append("--exiger-complet ne durcit pas la classe MANQUANTE")

    if echecs:
        for e in echecs:
            print(f"  ✗ {e}")
        print("SELF-TEST: ÉCHEC")
        return 1
    print("SELF-TEST: ok — 7 garanties (périmée, manquante, champs, silence sur "
          "cartes identiques, renommage de type toléré, palier non bloquant, "
          "palier durci)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--web", help="racine du dépôt tune-web-client")
    ap.add_argument(
        "--exiger-complet",
        action="store_true",
        help="palier 2 : faire échouer aussi sur les routes absentes de la carte",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.web:
        print("--web est requis (ou --self-test)", file=sys.stderr)
        return 2

    racine_web = Path(args.web).resolve()
    api_ts = racine_web / "src" / "lib" / "api.ts"
    if not api_ts.exists():
        print(f"{api_ts} introuvable — chemin du client web incorrect", file=sys.stderr)
        return 2
    if not CARTE_COMMITEE.exists():
        print(f"{CARTE_COMMITEE} introuvable", file=sys.stderr)
        return 2

    cartographe = _charger_cartographe()
    sources = {}
    for f in (racine_web / "src").rglob("*.ts"):
        try:
            sources[str(f.relative_to(racine_web))] = f.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            pass
    entrees, non_resolus = cartographe.carte(
        sources, api_ts.read_text(encoding="utf-8", errors="ignore")
    )
    if not entrees:
        print("aucune route extraite du client web — le motif d'appel a changé, "
              "ce contrôle ne garde plus rien", file=sys.stderr)
        return 2

    commitee = json.loads(CARTE_COMMITEE.read_text(encoding="utf-8"))
    regeneree = {"routes": entrees, "non_resolus": non_resolus}
    print(f"carte commitée : {len(commitee['routes'])} entrées ; "
          f"régénérée depuis {racine_web.name} : {len(entrees)} entrées")
    return rapporter(comparer(commitee, regeneree), args.exiger_complet)


if __name__ == "__main__":
    sys.exit(main())
