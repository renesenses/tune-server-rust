#!/usr/bin/env python3
"""Préflight : le client web n'appelle-t-il que des routes que le serveur sert ?

Le web et le serveur sortent dans la MÊME version, mais rien ne vérifiait leur
contrat. Deux fois en trois jours, un écran web fusionné a failli partir en
appelant une route serveur qui vivait encore dans une PR ouverte :

  - 0.9.82 : le Dynamic Range affiché côté web, ses tags lus côté serveur —
    les deux moitiés sont arrivées dans des versions différentes ;
  - 0.9.84 : l'écran Oxygen/Helium appelait `/library/albums-detailed`, route
    fournie par une PR non fusionnée. La release a été reportée à la main.

Le second cas a été rattrapé par une lecture humaine, pas par un contrôle. Ce
script en fait un contrôle.

CE QU'IL ATTRAPE
    Une route appelée par le web dont le segment distinctif n'apparaît nulle
    part dans les sources du serveur — donc, à coup sûr, une route non servie.

CE QU'IL N'ATTRAPE PAS, ET C'EST ASSUMÉ
    Une route qui existe mais répond autre chose que ce que le web attend
    (contrat de forme, pas de présence). C'était le cas de la découverte
    Bandcamp en 0.9.84 : les routes existaient, le plugin interrogeait la
    mauvaise API. Vérifier cela demanderait un schéma partagé — autre chantier.

    Les segments trop génériques (`status`, `list`, `search`…) sont ignorés :
    ils apparaissent partout et ne prouveraient rien. Le contrôle préfère se
    taire que crier au loup.

USAGE
    scripts/preflight-web-contract.py --web ../tune-web-client
    scripts/preflight-web-contract.py --self-test
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Répertoires de sources serveur où une route peut être déclarée : le serveur
# lui-même, le cœur, et les greffons in-tree (bandcamp, dj, karaoke…).
SOURCES_SERVEUR = ["tune-server/src", "tune-core/src", "plugins"]

# Segments qui ne distinguent rien : les retenir produirait du bruit, et un
# contrôle bruyant finit ignoré.
SEGMENTS_GENERIQUES = {
    "all", "cancel", "config", "create", "delete", "download", "get", "info",
    "list", "search", "set", "start", "state", "status", "stop", "update",
    "add", "remove", "clear", "reset", "test", "check", "sync", "scan",
}

# `${BASE}/library/albums-detailed?…` et `apiFetch('/appliance/storage')`.
MOTIF_TEMPLATE = re.compile(r"\$\{BASE\}(/[a-zA-Z0-9/_-]+)")
MOTIF_APIFETCH = re.compile(r"""apiFetch\(\s*['"`](/[a-zA-Z0-9/_-]+)""")


def routes_appelees_par_le_web(texte: str) -> set[str]:
    """Les chemins d'API littéraux qu'un source web appelle."""
    trouvees: set[str] = set()
    for motif in (MOTIF_TEMPLATE, MOTIF_APIFETCH):
        for m in motif.finditer(texte):
            chemin = m.group(1).rstrip("/")
            if chemin:
                trouvees.add(chemin)
    return trouvees


def segment_distinctif(route: str) -> str | None:
    """Le segment le plus spécifique d'une route, ou None s'il n'y en a pas.

    On remonte depuis la fin : `/library/albums-detailed` donne
    `albums-detailed`, tandis que `/converter/status` n'en donne aucun — son
    seul segment propre est générique, et le contrôle s'abstient.
    """
    for segment in reversed([s for s in route.strip("/").split("/") if s]):
        if segment.startswith("$") or segment.startswith(":"):
            continue  # paramètre d'URL, pas un nom de route
        if len(segment) > 3 and segment not in SEGMENTS_GENERIQUES:
            return segment
    return None


def routes_absentes(routes_web: set[str], sources_serveur: str) -> list[tuple[str, str]]:
    """Les routes appelées dont le segment distinctif est introuvable côté serveur."""
    absentes = []
    for route in sorted(routes_web):
        segment = segment_distinctif(route)
        if segment is None:
            continue
        if segment not in sources_serveur:
            absentes.append((route, segment))
    return absentes


def lire_sources_web(racine: Path) -> str:
    """Concatène les sources TypeScript/Svelte du client web."""
    morceaux = []
    for motif in ("*.ts", "*.svelte", "*.js"):
        for f in (racine / "src").rglob(motif):
            try:
                morceaux.append(f.read_text(encoding="utf-8", errors="ignore"))
            except OSError:
                pass
    return "\n".join(morceaux)


def lire_sources_serveur() -> str:
    """Concatène les sources Rust où des routes peuvent être déclarées."""
    morceaux = []
    for repertoire in SOURCES_SERVEUR:
        chemin = REPO_ROOT / repertoire
        if not chemin.exists():
            continue
        for f in chemin.rglob("*.rs"):
            try:
                morceaux.append(f.read_text(encoding="utf-8", errors="ignore"))
            except OSError:
                pass
    return "\n".join(morceaux)


def self_test() -> int:
    """Vérifie que le contrôle attrape ce qu'il doit, et se tait sinon."""
    echecs = []

    web = """
      export async function listAlbumsDetailed(p) {
        return fetchJSON(`${BASE}/library/albums-detailed?${p}`);
      }
      export const zones = () => apiFetch('/zones');
      export const etat = () => fetchJSON(`${BASE}/converter/status`);
    """
    routes = routes_appelees_par_le_web(web)
    if "/library/albums-detailed" not in routes:
        echecs.append("le gabarit ${BASE}/… n'est pas extrait")
    if "/zones" not in routes:
        echecs.append("apiFetch('/…') n'est pas extrait")

    # Le cas RÉEL de la 0.9.84 : la route manque côté serveur. Le serveur
    # fictif sert /zones et le convertisseur, mais pas albums-detailed.
    serveur_sans = (
        'Router::new().route("/zones", get(list_zones))\n'
        '.nest("/converter", converter::router())'
    )
    absentes = routes_absentes(routes, serveur_sans)
    noms = {r for r, _ in absentes}
    if "/library/albums-detailed" not in noms:
        echecs.append("une route absente du serveur n'est PAS signalée")
    if "/zones" in noms:
        echecs.append("une route pourtant servie est signalée à tort")
    if "/converter/status" in noms:
        echecs.append("/converter/status crie au loup alors que le convertisseur existe")

    # `status` seul ne doit jamais servir de segment distinctif.
    if segment_distinctif("/status") is not None:
        echecs.append("un chemin purement générique produit quand même un segment")

    # Contre-épreuve : la route ajoutée, plus aucune alerte.
    serveur_avec = serveur_sans + '\n.route("/library/albums-detailed", get(albums_detailed))'
    restantes = routes_absentes(routes, serveur_avec)
    if restantes:
        echecs.append(f"le contrôle reste rouge alors que tout est servi : {restantes}")

    if echecs:
        for e in echecs:
            print(f"  ✗ {e}")
        print("SELF-TEST: ÉCHEC")
        return 1
    print("SELF-TEST: ok — 5 garanties vérifiées (extraction gabarit, "
          "extraction apiFetch, détection, absence de bruit, contre-épreuve)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--web", help="racine du dépôt tune-web-client")
    ap.add_argument("--self-test", action="store_true", help="vérifie le contrôle lui-même")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    if not args.web:
        print("--web est requis (ou --self-test)", file=sys.stderr)
        return 2

    racine_web = Path(args.web).resolve()
    if not (racine_web / "src").is_dir():
        print(f"pas de src/ dans {racine_web} — chemin du client web incorrect", file=sys.stderr)
        return 2

    routes = routes_appelees_par_le_web(lire_sources_web(racine_web))
    if not routes:
        print("aucune route extraite du client web — le motif d'appel a changé, "
              "ce contrôle ne garde plus rien", file=sys.stderr)
        return 2

    absentes = routes_absentes(routes, lire_sources_serveur())

    print(f"routes appelées par le web : {len(routes)}")
    if not absentes:
        print("✓ toutes sont servies par le serveur")
        return 0

    print(f"✗ {len(absentes)} route(s) appelée(s) mais introuvable(s) côté serveur :")
    for route, segment in absentes:
        print(f"    {route}   (segment « {segment} »)")
    print()
    print("Publier en l'état livrerait ces écrans en appelant des routes absentes.")
    print("Vérifier qu'une PR serveur non fusionnée ne les fournit pas.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
