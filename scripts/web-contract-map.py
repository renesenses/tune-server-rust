#!/usr/bin/env python3
"""Lot 1 du chantier #1897 : la carte route → type → champs obligatoires.

Le client web déclare déjà ce qu'il attend de chaque route — 70 interfaces, 382
appels typés. Cette information existe, elle n'est simplement jamais confrontée
à la réalité. Ce script l'extrait sous une forme lisible par machine, pour que
le banc d'essai du lot 3 puisse s'en servir.

Il ne vérifie RIEN par lui-même. C'est un extracteur, et c'est voulu : un
extracteur qui se trompe est réparable, un extracteur qui juge est un contrôle
de plus à débuguer.

CE QU'IL PRODUIT
    Un JSON : pour chaque appel typé, la route, le type attendu, et les champs
    OBLIGATOIRES de ce type (ceux déclarés sans `?`).

CE QU'IL DIT QUAND IL NE SAIT PAS
    Les appels qu'il n'a pas su rattacher sont listés à part, jamais tus. Une
    carte silencieusement incomplète donnerait un banc d'essai qui se croit
    complet — exactement le défaut qu'on cherche à éliminer.

USAGE
    scripts/web-contract-map.py --web ../tune-web-client -o contrat-web.json
    scripts/web-contract-map.py --self-test
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# `fetchJSON<Zone[]>(`${BASE}/zones` … )` ou `fetchJSON<{ items: X[] }>(…)`.
# Le type peut contenir des accolades : on le capture non-greedy jusqu'à `>(`.
# La route est capturée JUSQU'AU guillemet fermant, et non par un jeu de
# caractères : `${encodeURIComponent(id)}` contient des parenthèses, et les
# exclure tronquait la route en plein milieu — 27 entrées sur 198 étaient
# amputées avant ce correctif. La normalisation vient après, sur la chaîne
# complète.
MOTIF_APPEL = re.compile(
    r"fetchJSON<(?P<type>.+?)>\(\s*(?P<q>[`'\"])\$\{BASE\}(?P<route>(?:(?!(?P=q)).)*)",
    re.S,
)

MOTIF_INTERFACE = re.compile(r"^\s*export\s+(?:interface|type)\s+(?P<nom>[A-Z][A-Za-z0-9_]*)\s*=?\s*\{", re.M)


def champs_du_bloc(bloc: str) -> tuple[list[str], list[str]]:
    """Champs de premier niveau d'un corps d'interface : (obligatoires, optionnels).

    Seule la profondeur 1 compte : un objet imbriqué décrit une sous-structure,
    pas un champ de la réponse.
    """
    obligatoires, optionnels = [], []
    profondeur = 0
    ligne_courante = []
    for car in bloc:
        if car in "{[(":
            profondeur += 1
        elif car in "}])":
            profondeur -= 1
        if car in ";\n" and profondeur == 0:
            texte = "".join(ligne_courante).strip()
            ligne_courante = []
            m = re.match(r"^(?P<nom>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<opt>\?)?\s*:", texte)
            if m:
                (optionnels if m.group("opt") else obligatoires).append(m.group("nom"))
            continue
        ligne_courante.append(car)
    # Le dernier champ n'a pas toujours de `;` final : sans ce vidage, il est
    # perdu — et un champ obligatoire manquant à la carte rendrait le banc
    # d'essai aveugle sur lui.
    texte = "".join(ligne_courante).strip()
    m = re.match(r"^(?P<nom>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<opt>\?)?\s*:", texte)
    if m:
        (optionnels if m.group("opt") else obligatoires).append(m.group("nom"))
    return obligatoires, optionnels


def corps_apres_accolade(texte: str, debut: int) -> str | None:
    """Le contenu entre l'accolade ouvrante en `debut` et sa fermante."""
    if debut >= len(texte) or texte[debut] != "{":
        return None
    profondeur = 0
    for i in range(debut, len(texte)):
        if texte[i] == "{":
            profondeur += 1
        elif texte[i] == "}":
            profondeur -= 1
            if profondeur == 0:
                return texte[debut + 1 : i]
    return None


def dictionnaire_des_types(sources: dict[str, str]) -> dict[str, dict]:
    """Nom de type → ses champs, pour tous les fichiers fournis."""
    types: dict[str, dict] = {}
    for chemin, texte in sources.items():
        for m in MOTIF_INTERFACE.finditer(texte):
            debut = texte.index("{", m.start())
            corps = corps_apres_accolade(texte, debut)
            if corps is None:
                continue
            obligatoires, optionnels = champs_du_bloc(corps)
            types[m.group("nom")] = {
                "obligatoires": obligatoires,
                "optionnels": optionnels,
                "declare_dans": chemin,
            }
    return types


def normaliser_route(route: str) -> str:
    """`/zones/${id}/dsp?x=1` → `/zones/{}/dsp` — la forme, pas les valeurs.

    Les interpolations sont remplacées d'abord : une route comme
    `/devices/${encodeURIComponent(id)}?x=1` doit devenir `/devices/{}`, et
    couper sur `?` avant de les réduire mutilerait celles qui en contiennent.
    """
    # Remplacement à accolades ÉQUILIBRÉES : une interpolation peut en
    # contenir une autre (`${qs ? `?${qs}` : ""}`), et une expression régulière
    # simple s'arrête à la première fermante — produisant une route fausse.
    sortie, i = [], 0
    while i < len(route):
        if route.startswith("${", i):
            profondeur, j = 0, i + 1
            while j < len(route):
                if route[j] == "{":
                    profondeur += 1
                elif route[j] == "}":
                    profondeur -= 1
                    if profondeur == 0:
                        break
                j += 1
            if j >= len(route):
                return ""  # interpolation non fermée : route indigne de confiance
            sortie.append("{}")
            i = j + 1
            continue
        sortie.append(route[i])
        i += 1
    route = "".join(sortie).split("?")[0]
    route = re.sub(r"\{\}+", "{}", route)
    return route.rstrip("/") or "/"


def carte(sources: dict[str, str], api_ts: str) -> tuple[list[dict], list[dict]]:
    """Construit la carte, et la liste de ce qui n'a pas pu être résolu."""
    types = dictionnaire_des_types(sources)
    entrees, non_resolus = [], []

    for m in MOTIF_APPEL.finditer(api_ts):
        brut = m.group("type").strip()
        # La méthode HTTP vit dans les options de l'appel, juste après l'URL :
        # `fetchJSON<T>(`${BASE}/x`, { method: 'POST', … })`. Sans elle, un GET
        # et un POST sur la même route sont confondus — et le banc d'essai
        # reproche à la réponse du GET les champs que le POST renvoie.
        # Fenêtre bornée, arrêtée au prochain appel pour ne pas lui voler sa
        # méthode.
        suite = api_ts[m.end(): m.end() + 400]
        coupe = suite.find("fetchJSON")
        if coupe != -1:
            suite = suite[:coupe]
        m_meth = re.search(r"method:\s*['\"`](GET|POST|PUT|PATCH|DELETE)['\"`]", suite, re.I)
        methode = m_meth.group(1).upper() if m_meth else "GET"
        route = normaliser_route(m.group("route"))
        if not route:
            non_resolus.append({
                "route": m.group("route")[:60], "type": brut,
                "raison": "interpolation non fermée — route non fiable",
            })
            continue
        nu = brut.removesuffix("[]").strip()
        # `import('./types').Zone` désigne un type parfaitement résoluble : la
        # forme d'import en ligne ne change rien à ce qu'il déclare. Trente
        # appels l'utilisent ; les rejeter aurait amputé la carte d'un cinquième.
        m_import = re.fullmatch(r"import\((?:'|\")[^'\"]+(?:'|\")\)\.([A-Z][A-Za-z0-9_]*)", nu)
        if m_import:
            nu = m_import.group(1)

        if nu.startswith("{"):
            corps = corps_apres_accolade(nu, 0)
            if corps is None:
                non_resolus.append({"route": route, "type": brut, "raison": "type en ligne illisible"})
                continue
            obligatoires, optionnels = champs_du_bloc(corps)
            entrees.append({
                "route": route, "methode": methode, "type": "(en ligne)",
                "liste": brut.endswith("[]"),
                "champs_obligatoires": obligatoires, "champs_optionnels": optionnels,
            })
            continue

        if not re.fullmatch(r"[A-Z][A-Za-z0-9_]*", nu):
            # `void`, `any`, `Record<…>`, unions… : rien d'exploitable.
            non_resolus.append({"route": route, "type": brut, "raison": "type non structurel"})
            continue

        connu = types.get(nu)
        if connu is None:
            non_resolus.append({"route": route, "type": brut, "raison": "type introuvable dans les sources"})
            continue

        entrees.append({
            "route": route, "methode": methode, "type": nu,
            "liste": brut.endswith("[]"),
            "champs_obligatoires": connu["obligatoires"],
            "champs_optionnels": connu["optionnels"],
        })

    # Dédoublonner : la même route peut être appelée plusieurs fois.
    vues, uniques = set(), []
    for e in entrees:
        cle = (e["route"], e["methode"], e["type"], tuple(e["champs_obligatoires"]))
        if cle in vues:
            continue
        vues.add(cle)
        uniques.append(e)
    return sorted(uniques, key=lambda e: (e["route"], e["methode"])), non_resolus


def self_test() -> int:
    echecs = []
    types_src = {
        "types.ts": """
            export interface Zone {
              id: number;
              name: string;
              volume?: number;
              sortie: { type: string; id: string };
            }
        """
    }
    api = """
      export const zones = () => fetchJSON<Zone[]>(`${BASE}/zones`);
      export const zone = (id) => fetchJSON<Zone>(`${BASE}/zones/${id}`);
      export const dr = (p) => fetchJSON<{ items: X[]; total: number }>(`${BASE}/library/albums-detailed?${p}`);
      export const rien = () => fetchJSON<void>(`${BASE}/system/ping`);
      export const inconnu = () => fetchJSON<PasDefini>(`${BASE}/x/y-z`);
      export const imp = () => fetchJSON<import('./types').Zone[]>(`${BASE}/zones-bis`);
      export const dev = (id) => fetchJSON<Zone>(`${BASE}/devices/${encodeURIComponent(id)}?x=1`);
      export const listP = () => fetchJSON<{ presets: Zone[] }>(`${BASE}/eq/presets`);
      export const newP = (b) => fetchJSON<Zone>(`${BASE}/eq/presets`, { method: 'POST', body: b });
    """
    entrees, non_resolus = carte(types_src, api)
    par_route = {e["route"]: e for e in entrees}

    if "/zones" not in par_route:
        echecs.append("un type nommé n'est pas résolu")
    elif par_route["/zones"]["champs_obligatoires"] != ["id", "name", "sortie"]:
        echecs.append(f"champs obligatoires faux : {par_route['/zones']['champs_obligatoires']}")
    elif par_route["/zones"]["champs_optionnels"] != ["volume"]:
        echecs.append("un champ optionnel est compté comme obligatoire")

    if not par_route.get("/zones")["liste"]:
        echecs.append("Zone[] n'est pas marqué comme liste")

    if "/zones/{}" not in par_route:
        echecs.append("un paramètre d'URL casse la normalisation de la route")

    if "/library/albums-detailed" not in par_route:
        echecs.append("un type en ligne n'est pas exploité")
    elif sorted(par_route["/library/albums-detailed"]["champs_obligatoires"]) != ["items", "total"]:
        echecs.append("les champs d'un type en ligne sont mal lus")

    if "/zones-bis" not in par_route:
        echecs.append("la forme import('./types').X n'est pas résolue")
    elif par_route["/zones-bis"]["champs_obligatoires"] != ["id", "name", "sortie"]:
        echecs.append("la forme import('./types').X résout le mauvais type")

    if "/devices/{}" not in par_route:
        echecs.append(f"encodeURIComponent tronque la route : {sorted(par_route)}")

    eq = {e["methode"]: e for e in entrees if e["route"] == "/eq/presets"}
    if set(eq) != {"GET", "POST"}:
        echecs.append(f"GET et POST sur la même route ne sont pas distingués : {sorted(eq)}")
    elif eq["GET"]["champs_obligatoires"] != ["presets"]:
        echecs.append("le GET hérite des champs du POST")

    raisons = {n["route"]: n["raison"] for n in non_resolus}
    if "/system/ping" not in raisons:
        echecs.append("un type non structurel (void) n'est pas signalé")
    if "/x/y-z" not in raisons:
        echecs.append("un type introuvable n'est PAS signalé — la carte se croirait complète")

    if echecs:
        for e in echecs:
            print(f"  ✗ {e}")
        print("SELF-TEST: ÉCHEC")
        return 1
    print("SELF-TEST: ok — 11 garanties (type nommé, optionnels, liste, paramètre "
          "d'URL, type en ligne, import en ligne, route à parenthèses, méthode HTTP, et les deux non-résolutions)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--web", help="racine du dépôt tune-web-client")
    ap.add_argument("-o", "--out", help="fichier JSON de sortie (défaut : stdout)")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.web:
        print("--web est requis (ou --self-test)", file=sys.stderr)
        return 2

    racine = Path(args.web).resolve()
    api_path = racine / "src" / "lib" / "api.ts"
    if not api_path.exists():
        print(f"{api_path} introuvable — chemin du client web incorrect", file=sys.stderr)
        return 2

    sources = {}
    for f in (racine / "src").rglob("*.ts"):
        try:
            sources[str(f.relative_to(racine))] = f.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            pass

    entrees, non_resolus = carte(sources, api_path.read_text(encoding="utf-8", errors="ignore"))

    sortie = {
        "routes": entrees,
        "non_resolus": non_resolus,
        "resume": {
            "routes_cartographiees": len(entrees),
            "appels_non_resolus": len(non_resolus),
        },
    }
    texte = json.dumps(sortie, indent=2, ensure_ascii=False)
    if args.out:
        Path(args.out).write_text(texte + "\n", encoding="utf-8")
        print(f"{len(entrees)} routes cartographiées, {len(non_resolus)} appels non résolus → {args.out}")
    else:
        print(texte)
    return 0


if __name__ == "__main__":
    sys.exit(main())
