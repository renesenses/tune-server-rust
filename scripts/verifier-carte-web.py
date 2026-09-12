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

LES QUATRE CLASSES D'ÉCART, ET CE QU'ELLES COÛTENT
    DISPARUE  La carte cite une route que le client n'appelle NULLE PART. Le
              banc d'essai fait alors respecter au serveur un contrat que plus
              personne ne lit : il bloque des changements légitimes et fige du
              code mort. C'est le cas de `/library/tracks/{}/lyrics`, où la
              carte recopiait un type sans appelant (#3002).
              → BLOQUANTE.

    DÉTYPÉE   Le client appelle TOUJOURS la route, mais sa forme d'appel n'est
              plus cartographiable : `fetchJSON<any>`, `fetchVoid`, ou un
              changement de méthode. La route n'est pas morte — son type l'est,
              et avec lui la garde.
              → SIGNALÉE, PAS BLOQUANTE : le remède est de rendre un type au
              client, pas de régénérer la carte.

              Cette classe existe parce que la première version de ce contrôle
              ne l'avait pas : elle annonçait « le client n'appelle plus » pour
              DIX routes, dont TROIS étaient toujours appelées
              (`POST /system/music-dirs` passée à `fetchJSON<any>`,
              `POST /zones/{}/queue/move` à `fetchVoid`, `/zones/{}/share`
              passée de GET à POST). Trois accusations fausses sur dix
              suffisent à faire ignorer un contrôle.

    MANQUANTE Le client appelle une route que la carte ignore. Le banc d'essai
              ne la joue jamais, donc aucune dérive de champs n'y est visible.
              C'est exactement par ce trou qu'est passé #3002.
              → SIGNALÉE, PAS BLOQUANTE (voir « paliers » plus bas).

    CHAMPS    Route connue des deux côtés, exigences différentes.
              → SIGNALÉE, PAS BLOQUANTE.

POURQUOI UNE SEULE CLASSE BLOQUE POUR COMMENCER
    Mesure du 11/09/2026, carte du 31/08 contre le client web du jour : 7
    disparues, 3 détypées, 30 manquantes, 3 contrats de champs divergents.
    Rendre les quatre classes bloquantes d'emblée aurait produit un rouge
    permanent, et un contrôle qui rougit toujours finit par être contourné —
    ce dépôt en a déjà fait deux fois les frais.

    La classe DISPARUE retombe à zéro dès que la carte est régénérée, et n'y
    remonte que lorsqu'un écran cesse d'appeler une route. Elle bloque donc
    rarement, et quand elle bloque elle a raison.

    Palier suivant, à armer quand la carte sera régénérée à chaque bump web :
    passer MANQUANTE en bloquante avec `--exiger-complet`.

CE QUE CE CONTRÔLE NE REGARDE PAS
    Le serveur. Il compare deux cartes, toutes deux tirées du CLIENT web : la
    commitée et celle régénérée depuis le dépôt web. Aucun chemin serveur n'y
    est résolu, donc aucun `nest()` n'y intervient. Le versant serveur — un
    chemin cité par la carte que le routeur ne sert plus — est gardé ailleurs,
    par `tune-server/tests/web_response_contracts.rs`, qui interroge le routeur
    ASSEMBLÉ et non les sources.

USAGE
    scripts/verifier-carte-web.py --web ../tune-web-client
    scripts/verifier-carte-web.py --web ../tune-web-client --exiger-complet
    scripts/verifier-carte-web.py --self-test
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
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


def detecteur_d_appel(texte_web: str, routes_non_resolues: set[str]):
    """« Cette route est-elle encore appelée quelque part par le client ? »

    Deux sources, parce qu'une seule mentirait :

    - `routes_non_resolues` : le cartographe a VU l'appel mais n'a pas su typer
      sa réponse (`fetchJSON<any>`). Il le dit, au lieu de se taire — c'est
      exactement à ça que sert sa liste `non_resolus`.
    - la recherche littérale, pour les formes qu'il ne voit pas du tout :
      `fetchVoid`, `fetch` nu, appel déplacé dans un composant.

    Le motif est reconstruit depuis la route NORMALISÉE : `/zones/{}/quality`
    redevient `/zones/${…}/quality`. Comparer des chaînes brutes échouerait,
    puisque la carte ne garde que la forme.
    """
    motifs: dict[str, re.Pattern] = {}

    def encore_appelee(route: str) -> bool:
        if route in routes_non_resolues:
            return True
        if route not in motifs:
            morceaux = [
                re.escape(s) if s != "{}" else r"\$\{[^`'\"]+?\}"
                for s in route.strip("/").split("/")
                if s
            ]
            motifs[route] = re.compile("/" + "/".join(morceaux)) if morceaux else None
        motif = motifs[route]
        return bool(motif and motif.search(texte_web))

    return encore_appelee


def comparer(
    commitee: dict,
    regeneree: dict,
    encore_appelee=lambda route: False,
) -> dict[str, list]:
    """Les trois classes d'écart entre la carte du dépôt et celle du client.

    La classe PÉRIMÉE se juge sur la ROUTE, pas sur la clef complète : un type
    renommé côté web (`RechercheRadios` → `StreamingSearchResult`) déplace
    l'entrée sans que la route cesse d'être appelée. Confondre les deux ferait
    rougir un simple renommage, et ce contrôle serait ignoré en une semaine.

    `encore_appelee` sépare DISPARUE de DÉTYPÉE. Sans elle, la première version
    de ce contrôle annonçait « le client n'appelle plus » pour dix routes, alors
    que TROIS étaient toujours appelées — seulement plus cartographiables :
    `POST /system/music-dirs` est passée à `fetchJSON<any>`,
    `POST /zones/{}/queue/move` à `fetchVoid`, et `/zones/{}/share` a changé de
    méthode. Trois accusations fausses sur dix suffisent à faire ignorer un
    contrôle. Le remède n'est pas le même : une route disparue veut une carte
    régénérée, une route détypée veut un type rendu au client.
    """
    anciennes = {cle(e): e for e in commitee["routes"]}
    nouvelles = {cle(e): e for e in regeneree["routes"]}
    routes_web = {(e["methode"], e["route"]) for e in regeneree["routes"]}
    routes_carte = {(e["methode"], e["route"]) for e in commitee["routes"]}

    hors_carte = sorted(routes_carte - routes_web)
    perimees = [(m, r) for m, r in hors_carte if not encore_appelee(r)]
    detypees = [(m, r) for m, r in hors_carte if encore_appelee(r)]
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
    return {
        "perimees": perimees,
        "detypees": detypees,
        "manquantes": manquantes,
        "champs": champs,
    }


def rapporter(ecarts: dict[str, list], exiger_complet: bool) -> int:
    perimees, detypees, manquantes, champs = (
        ecarts["perimees"],
        ecarts["detypees"],
        ecarts["manquantes"],
        ecarts["champs"],
    )
    print(
        f"carte vs client web : {len(perimees)} disparue(s), "
        f"{len(detypees)} détypée(s), {len(manquantes)} manquante(s), "
        f"{len(champs)} contrat(s) de champs divergent(s)"
    )

    for methode, route in detypees:
        print(f"::warning::contrat détypé : {methode} {route} est TOUJOURS appelée par "
              f"le web, mais sa forme d'appel n'est plus cartographiable "
              f"(`fetchJSON<any>`, `fetchVoid`, ou changement de méthode). La route "
              f"n'est pas morte : c'est son TYPE qui a disparu, et avec lui la garde")
    for methode, route in manquantes:
        print(f"::warning::carte incomplète : {methode} {route} est appelée par le web, "
              f"absente de docs/contrat-web.json — aucune dérive n'y est visible")
    for (methode, route, type_web), retires, ajoutes in champs:
        print(f"::warning::contrat divergent : {methode} {route} ({type_web}) "
              f"— la carte exige en trop {retires or '[]'}, ignore {ajoutes or '[]'}")

    if perimees:
        print()
        print(f"✗ {len(perimees)} route(s) citée(s) par docs/contrat-web.json que le "
              f"client web n'appelle NULLE PART :")
        for methode, route in perimees:
            print(f"    {methode} {route}")
        print()
        print("Le banc d'essai `web_response_contracts` fait respecter au serveur un")
        print("contrat que plus aucun écran ne lit. Régénérer la carte :")
        print("    scripts/web-contract-map.py --web <tune-web-client> -o docs/contrat-web.json")
        return 1

    print("✓ aucune route disparue dans docs/contrat-web.json")
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
        echecs.append("une route que le web n'appelle plus n'est PAS signalée disparue")
    if ("GET", neuve) not in ecarts["manquantes"]:
        echecs.append("une route appelée par le web et absente de la carte n'est pas signalée")
    if not any(k[1] == stable for k, _, _ in ecarts["champs"]):
        echecs.append("un champ obligatoire ajouté par le web ne produit aucun écart")
    if rapporter(ecarts, exiger_complet=False) != 1:
        echecs.append("une route disparue ne fait PAS échouer le contrôle")

    # ── La classe DÉTYPÉE, celle dont l'absence a produit trois fausses
    # accusations sur dix ───────────────────────────────────────────────────
    #
    # Même carte, même absence de la carte régénérée — mais la route est
    # TOUJOURS appelée par le client. Elle doit sortir de la classe bloquante.
    detypee = comparer(commitee, regeneree, encore_appelee=lambda r: r == morte)
    if detypee["perimees"]:
        echecs.append(
            f"une route toujours appelée est accusée d'avoir disparu : {detypee['perimees']}"
        )
    if ("POST", morte) not in detypee["detypees"]:
        echecs.append("une route détypée n'est pas signalée du tout — elle disparaît en silence")
    if rapporter(detypee, exiger_complet=False) != 0:
        echecs.append("une route détypée bloque alors qu'elle ne devrait que prévenir")

    # Le détecteur d'appel lui-même, sur les trois formes qui l'ont pris en
    # défaut. Le texte est ASSEMBLÉ pour que ce fichier ne se trouve pas
    # lui-même s'il tombait un jour dans le champ de lecture du cartographe.
    base = "$" + "{BASE}"
    interp = "$" + "{zoneId}"
    texte = "\n".join([
        f"fetchVoid(`{base}/zones/{interp}/queue/move`, {{ method: 'POST' }})",
        f"fetchJSON<any>(`{base}/system/music-dirs`, {{ method: 'POST' }})",
    ])
    detecte = detecteur_d_appel(texte, {"/system/music-dirs"})
    if not detecte("/zones/{}/queue/move"):
        echecs.append("le détecteur rate un appel `fetchVoid` : la route serait dite disparue")
    if not detecte("/system/music-dirs"):
        echecs.append("le détecteur ignore la liste `non_resolus` du cartographe")
    if detecte("/" + "/".join(["zones", "{}", "jamais-appelee"])):
        echecs.append("le détecteur voit des appels qui n'existent pas — plus rien ne bloquerait")

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
    print("SELF-TEST: ok — 14 garanties (disparue bloquante, manquante, champs, "
          "détypée signalée et NON bloquante, détypée jamais comptée disparue, "
          "détecteur sur `fetchVoid`, détecteur sur `non_resolus`, détecteur "
          "muet sur une route inexistante, cartes identiques, renommage de type "
          "toléré, palier non bloquant, palier durci)")
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

    # Le détecteur d'appel lit AUSSI les `.svelte` : une route peut avoir quitté
    # `api.ts` pour un composant sans cesser d'exister. La déclarer disparue
    # sur la seule lecture d'`api.ts` serait une accusation fausse.
    texte_web = "\n".join(
        list(sources.values())
        + [
            f.read_text(encoding="utf-8", errors="ignore")
            for motif in ("*.svelte", "*.js")
            for f in (racine_web / "src").rglob(motif)
        ]
    )
    encore_appelee = detecteur_d_appel(
        texte_web, {n.get("route") for n in non_resolus if n.get("route")}
    )

    print(f"carte commitée : {len(commitee['routes'])} entrées ; "
          f"régénérée depuis {racine_web.name} : {len(entrees)} entrées")
    return rapporter(
        comparer(commitee, regeneree, encore_appelee), args.exiger_complet
    )


if __name__ == "__main__":
    sys.exit(main())
