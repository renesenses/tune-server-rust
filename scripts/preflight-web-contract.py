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
    1. Une route appelée par le web dont le segment distinctif n'apparaît nulle
       part dans les sources du serveur — donc, à coup sûr, une route non servie.

    2. Une route dont le PRÉFIXE est monté par un module qui ne la déclare pas.
       Le contrôle (1) cherche un mot n'importe où : il ne sait pas dire QUI
       sert le chemin. `/metadata/duplicates` passait au vert parce que
       `/library/duplicates` existe, et `/metadata/mp3/repair` parce que le
       serveur contient le mot `repaired`. Sept 404 supplémentaires dormaient
       derrière ce trou (#2004), en plus des six de #1893.

       Ce second contrôle est délibérément timide : dès qu'un module ne se
       résout pas avec certitude — composition `.merge()`, préfixe non monté,
       fichier introuvable — il se tait. Un contrôle qui accuse à tort est plus
       nuisible qu'un contrôle qui rate : on apprend à l'ignorer.

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
# lui-même, le cœur, les greffons in-tree (bandcamp, dj, karaoke…), et les
# caisses HTTP extraites de `tune-server`.
#
# ⚠️ Cette liste a DÉJÀ été prise en défaut. Le 01/09/2026, la v0.9.130 a été
# refusée sur deux routes `/smart-ai/*` parfaitement servies : #2541 venait de
# sortir `smart_ai.rs` de `tune-server/src/routes/` vers `tune-smart-http`, et
# le scanner cherchait encore à l'ancien endroit. Soixante-dix déclarations de
# route lui étaient devenues invisibles, réparties sur quatre caisses.
#
# Un contrôle qui crie au loup sur du code juste est pire qu'absent : il
# apprend à passer outre. D'où la garde `caisses_a_routes_non_scannees()`
# ci-dessous, qui refuse de tourner si une caisse déclare des routes sans
# figurer ici. Le prochain déplacement de module sera signalé PAR le script,
# nommément, au lieu d'être découvert un jour de release.
SOURCES_SERVEUR = [
    "tune-server/src",
    "tune-core/src",
    "tune-smart-http/src",
    "tune-stream-http/src",
    "tune-streaming-http/src",
    "tune-bridge/src",
    "plugins",
]

# Routes déjà cassées AVANT ce contrôle, documentées dans #1893.
#
# Elles partent en 404 depuis un certain temps : ce n'est pas ce train qui les
# a introduites. Les laisser rougir en permanence apprendrait à ignorer le
# contrôle — et un contrôle qu'on ignore ne garde rien. Elles sont donc
# tolérées NOMMÉMENT, jamais par une règle vague : toute NOUVELLE route absente
# fait échouer le préflight.
#
# Retirer une ligne d'ici dès que la route est servie ou l'appel supprimé.
# SIX ENTRÉES ONT ÉTÉ RETIRÉES le 20/08, la dette étant payée — c'est le
# script lui-même qui les a signalées, via sa ligne « est désormais servie » :
#   /metadata/auto-fix, /auto-fix/status, /reclassify-genres-by-path  (#1920)
#   /library/import/{roon,plex,playlists}                             (#520 web)
SOCLE_CONNU: dict[str, str] = {
    # VIDE, et c'est l'objectif atteint.
    #
    # Ce socle a compte jusqu'a 18 entrees. Chacune a ete soldee : cinq routes
    # ecrites (un moteur existait, la porte HTTP manquait), six appels corriges
    # (la fonction etait la, sous une autre adresse), cinq fonctions mortes
    # supprimees (tune-web-client#543), une remplacee par un refus explicite.
    #
    # Le controle ne tolere donc plus RIEN : toute route appelee par le web et
    # absente du serveur le fait echouer, sans exemption ni liste a maintenir.
    #
    # Reouvrir ce socle est une decision, pas une commodite. Une entree se
    # justifie par un numero d'issue et se retire des que la dette est payee —
    # un socle qui ne maigrit jamais finit par tout tolerer, et un controle qui
    # tolere tout ne garde rien.
}

# Segments qui ne distinguent rien : les retenir produirait du bruit, et un
# contrôle bruyant finit ignoré.
SEGMENTS_GENERIQUES = {
    "all", "cancel", "config", "create", "delete", "download", "get", "info",
    "list", "search", "set", "start", "state", "status", "stop", "update",
    "add", "remove", "clear", "reset", "test", "check", "sync", "scan",
}

# `${BASE}/library/albums-detailed?…` et `apiFetch('/appliance/storage')`.
#
# Le point fait partie du chemin : `/radios/export.m3u` tronqué à
# `/radios/export` désignait une route qui n'existe pas, et le contrôle
# signalait un défaut imaginaire.
MOTIF_TEMPLATE = re.compile(r"\$\{BASE\}(/[a-zA-Z0-9/._-]+)")
MOTIF_APIFETCH = re.compile(r"""apiFetch\(\s*['"`](/[a-zA-Z0-9/._-]+)""")


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


# ── Second contrôle : le PRÉFIXE, pas seulement le segment ───────────────────
#
# Le contrôle par segment cherche un mot n'importe où dans les sources serveur.
# Il ne sait donc pas dire QUI sert la route. `/metadata/duplicates` passe au
# vert parce que `/library/duplicates` existe — le préfixe est faux, l'appel
# part en 404, et le contrôle est muet. Pire, `repair` est trouvé parce que le
# serveur contient le mot `repaired` : une sous-chaîne quelconque suffit.
#
# Ce second contrôle compare le chemin ENTIER aux routes du module qui sert
# réellement ce préfixe. Il reste volontairement timide : dès qu'un module ne
# se résout pas avec certitude, il se tait plutôt que d'inventer une alerte.

MOTIF_NEST = re.compile(r'\.nest\(\s*"(/[a-zA-Z0-9/_-]+)"\s*,\s*([a-zA-Z0-9_:]+)')
MOTIF_ROUTE = re.compile(r'\.route\(\s*"(/[a-zA-Z0-9/_{}-]*)"')
# Un module qui compose d'autres routeurs ne peut pas être inventorié à plat.
MOTIF_COMPOSITION = re.compile(r"\.merge\(|\.nest\(|\.nest_service\(|\.fallback\(")


def prefixes_montes(racine_serveur: Path) -> dict[str, str]:
    """Les préfixes `.nest("/x", module::router)` et le module qui les sert.

    Un préfixe monté sur une COMPOSITION est écarté. `/appliance` vaut
    `appliance::router().merge(appliance_storage::router())` : ne retenir que
    le premier module fait manquer six routes bien servies, et le contrôle
    accuse à tort. Mieux vaut ne rien dire de ce préfixe.
    """
    montes: dict[str, str] = {}
    composites: set[str] = set()

    for f in racine_serveur.rglob("*.rs"):
        try:
            texte = f.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        for m in MOTIF_NEST.finditer(texte):
            prefixe, cible = m.group(1), m.group(2)
            # La suite immédiate de l'expression : `.merge(…)` s'y trouve, le
            # cas échéant, avant toute autre déclaration de route.
            suite = texte[m.end():m.end() + 200]
            coupure = min(
                (i for i in (suite.find(".nest("), suite.find(".route(")) if i >= 0),
                default=len(suite),
            )
            if ".merge(" in suite[:coupure]:
                composites.add(prefixe)
                continue

            morceaux = [p for p in cible.split("::") if p not in ("crate", "router")]
            if morceaux:
                montes.setdefault(prefixe, morceaux[-1])

    for prefixe in composites:
        montes.pop(prefixe, None)
    return montes


def fichiers_du_module(racine_serveur: Path, module: str) -> list[Path]:
    """Les sources du module qui définit `router()` : `x.rs`, ou `x/` entier.

    On EXIGE de voir `fn router` dans le fichier retenu. Sans cette exigence,
    `rglob` ramenait n'importe quel fichier au nom voisin — pour `/appliance`
    il manquait `appliance_storage.rs`, et le contrôle déclarait absentes des
    routes parfaitement servies. Un module qu'on ne sait pas localiser avec
    certitude doit rendre le contrôle muet, pas bavard.
    """
    fichiers: list[Path] = []

    for base in racine_serveur.rglob(f"{module}.rs"):
        try:
            if "fn router" in base.read_text(encoding="utf-8", errors="ignore"):
                fichiers.append(base)
        except OSError:
            pass

    for repertoire in racine_serveur.rglob(module):
        if not repertoire.is_dir():
            continue
        mod_rs = repertoire / "mod.rs"
        try:
            if mod_rs.is_file() and "fn router" in mod_rs.read_text(encoding="utf-8", errors="ignore"):
                fichiers.extend(repertoire.rglob("*.rs"))
        except OSError:
            pass

    return fichiers


def routes_du_module(fichiers: list[Path]) -> tuple[set[str], bool]:
    """Les routes déclarées par un module, et s'il compose d'autres routeurs.

    Le second drapeau est un aveu d'ignorance : un module qui `merge` ou `nest`
    sert des chemins que cet inventaire à plat ne voit pas, donc son absence ne
    prouve rien et le contrôle doit se taire.
    """
    routes: set[str] = set()
    compose = False
    for f in fichiers:
        try:
            texte = f.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        routes.update(MOTIF_ROUTE.findall(texte))
        if MOTIF_COMPOSITION.search(texte):
            compose = True
    return routes, compose


def _segments(chemin: str) -> list[str]:
    return [s for s in chemin.strip("/").split("/") if s]


def _servi_par(reste: str, routes: set[str]) -> bool:
    """Le module sert-il ce chemin ?

    Deux tolérances, chacune apprise d'un faux positif réel :

    - Un paramètre serveur accepte n'importe quel segment littéral. Le serveur
      déclare `/{service}/auth/status` ; le web appelle `/youtube/auth/status`.
      C'est la MÊME route, et les comparer littéralement inventait un défaut.

    - L'extraction web s'arrête au premier caractère non littéral, donc un
      appel paramétré `${BASE}/metadata/tracks/${id}` ne donne que
      `/metadata/tracks`. Un chemin web plus COURT qu'une route serveur qu'il
      préfixe est donc servi, pas manquant.
    """
    cible = _segments(reste)
    for route in routes:
        connue = _segments(route)
        if len(cible) > len(connue):
            continue
        if all(s.startswith("{") or s == c for c, s in zip(cible, connue)):
            return True
    return False


def routes_de_prefixe_absentes(
    routes_web: set[str], racine_serveur: Path
) -> list[tuple[str, str]]:
    """Les chemins web dont le module servant le préfixe ne déclare pas la route."""
    montes = prefixes_montes(racine_serveur)
    if not montes:
        return []  # rien compris au montage : se taire.

    cache: dict[str, tuple[set[str], bool]] = {}
    absentes: list[tuple[str, str]] = []

    for chemin in sorted(routes_web):
        # Le préfixe le plus long qui corresponde : `/library/smart-playlists`
        # avant `/library`.
        candidats = [p for p in montes if chemin == p or chemin.startswith(p + "/")]
        if not candidats:
            continue
        prefixe = max(candidats, key=len)
        module = montes[prefixe]

        if module not in cache:
            cache[module] = routes_du_module(fichiers_du_module(racine_serveur, module))
        routes, compose = cache[module]

        # Un module vide n'a pas été trouvé ; un module composite cache des
        # chemins. Dans les deux cas, l'absence ne prouve rien.
        if not routes or compose:
            continue

        reste = chemin[len(prefixe):] or "/"
        if not _servi_par(reste, routes):
            absentes.append((chemin, f"{prefixe} → module « {module} »"))

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


def caisses_a_routes_non_scannees() -> list[tuple[str, int]]:
    """Les caisses de l'espace de travail qui déclarent des routes SANS être scannées.

    La garde qui manquait le 01/09/2026. `SOURCES_SERVEUR` est une liste tenue
    à la main : le jour où un module déménage vers une caisse qui n'y figure
    pas, ses routes deviennent invisibles et le préflight refuse une release
    parfaitement saine — en accusant le client web, qui n'y est pour rien.

    Plutôt que de faire confiance à la liste, on la CONFRONTE à l'arbre : toute
    caisse `tune-*/src` qui contient `.route(` doit être scannée. Le contrôle
    ne coûte qu'une lecture des sources déjà sur disque.

    Rend la liste des caisses fautives avec leur nombre de déclarations, vide
    si tout est couvert.
    """
    couvertes = {r.split("/", 1)[0] for r in SOURCES_SERVEUR}
    fautives: list[tuple[str, int]] = []
    for caisse in sorted(REPO_ROOT.glob("tune-*")):
        if not caisse.is_dir() or caisse.name in couvertes:
            continue
        src = caisse / "src"
        if not src.is_dir():
            continue
        compte = 0
        for f in src.rglob("*.rs"):
            try:
                compte += f.read_text(encoding="utf-8", errors="ignore").count(".route(")
            except OSError:
                pass
        if compte:
            fautives.append((caisse.name, compte))
    return fautives


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

    # Le socle se vide LÉGITIMEMENT à mesure que les routes sont servies : cette
    # garde était épinglée sur `/metadata/auto-fix`, et elle a cassé le jour où
    # cette route a enfin été écrite (#1920). Épingler une clef précise punit la
    # réparation. On vérifie donc la FORME — un socle vidé d'un coup, ou dont
    # une entrée ne référence plus rien, reste attrapé.
    # Un socle VIDE est l'etat vise, pas une anomalie. Cette garde exigeait
    # qu'il soit non vide — elle a donc casse le jour ou la derniere dette a
    # ete payee. C'est la DEUXIEME fois qu'une garde de ce fichier punit la
    # reparation : la precedente etait epinglee sur une clef precise et a casse
    # quand cette route a enfin ete ecrite. Une garde ne doit jamais rendre le
    # succes plus couteux que le statu quo.
    #
    # Il ne reste donc que la FORME : ce qui est tolere doit ressembler a un
    # chemin et porter un numero d'issue. Rien a verifier quand il n'y a rien.
    for route, ref in SOCLE_CONNU.items():
        if not route.startswith("/"):
            echecs.append(f"socle : « {route} » n'est pas un chemin")
        if not re.fullmatch(r"#\d+", ref):
            echecs.append(f"socle : « {route} » tolérée sans numéro d'issue ({ref!r})")

    echecs.extend(self_test_prefixe())

    if echecs:
        for e in echecs:
            print(f"  ✗ {e}")
        print("SELF-TEST: ÉCHEC")
        return 1
    print("SELF-TEST: ok — 12 garanties vérifiées (extraction gabarit, "
          "extraction apiFetch, détection, absence de bruit, contre-épreuve, socle connu, "
          "mauvais préfixe, sous-chaîne trompeuse, appel paramétré, module composite, "
          "module introuvable, préfixe le plus long)")
    return 0


def self_test_prefixe() -> list[str]:
    """Le contrôle par préfixe attrape-t-il ce que celui par segment rate ?

    Chaque cas est un défaut RÉEL observé sur #1893, pas une hypothèse.
    """
    import tempfile

    echecs: list[str] = []

    with tempfile.TemporaryDirectory() as tmp:
        racine = Path(tmp)
        (racine / "routes").mkdir()
        (racine / "routes" / "mod.rs").write_text(
            'Router::new()\n'
            '    .nest("/metadata", metadata::router())\n'
            '    .nest("/library", library::router())\n'
            '    .nest("/library/smart-playlists", smart_playlists::router())\n'
            '    .nest("/cloud", cloud::router())\n',
            encoding="utf-8",
        )
        # Le module qui sert /metadata : pas de doublons, pas de mp3.
        (racine / "routes" / "metadata.rs").write_text(
            'pub fn router() -> Router {\n'
            '    Router::new()\n'
            '        .route("/suggestions", get(list_suggestions))\n'
            '        .route("/tracks/{id}/edit", post(edit_track))\n'
            '        .route(\n'
            '            "/fix-genres-by-artist-fuzzy",\n'
            '            post(fix_fuzzy),\n'
            '        )\n'
            '}\n'
            'fn repaired() {}\n',  # le mot « repair » existe, la route non
            encoding="utf-8",
        )
        # Le module qui sert /library : c'est LUI qui a les doublons.
        (racine / "routes" / "library").mkdir()
        (racine / "routes" / "library" / "mod.rs").write_text(
            'pub fn router() -> Router {\n'
            '    Router::new().route("/duplicates", get(list_duplicates))\n'
            '}\n',
            encoding="utf-8",
        )
        # Un module composite : son inventaire à plat est incomplet, donc muet.
        (racine / "routes" / "cloud.rs").write_text(
            'pub fn router() -> Router {\n'
            '    Router::new().route("/status", get(s)).merge(sous::router())\n'
            '}\n',
            encoding="utf-8",
        )
        (racine / "routes" / "smart_playlists.rs").write_text(
            'pub fn router() -> Router { Router::new().route("/", get(l)) }\n',
            encoding="utf-8",
        )

        appels = {
            "/metadata/duplicates",                 # servi sous /library → 404
            "/metadata/mp3/repair",                 # « repaired » existe, pas la route
            "/metadata/suggestions",                # servi
            "/metadata/tracks",                     # appel paramétré tronqué → servi
            "/metadata/fix-genres-by-artist-fuzzy",  # déclaré sur plusieurs lignes → servi
            "/library/duplicates",                  # servi
            "/cloud/inconnu",                       # module composite → silence
            "/inconnu/total",                       # préfixe non monté → silence
            "/library/smart-playlists",             # préfixe le plus long gagne
        }
        signalees = {r for r, _ in routes_de_prefixe_absentes(appels, racine)}

        attendus = {"/metadata/duplicates", "/metadata/mp3/repair"}
        for r in attendus:
            if r not in signalees:
                echecs.append(f"préfixe : {r} aurait dû être signalé")
        for r in signalees - attendus:
            echecs.append(f"préfixe : {r} signalé à tort (bruit)")

        if not prefixes_montes(racine):
            echecs.append("préfixe : aucun montage .nest reconnu")

    return echecs


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

    # AVANT de juger le client web, vérifier qu'on sait lire le serveur. Un
    # scanner aveugle à une caisse accuse l'écran d'appeler une route absente
    # alors qu'elle est servie — c'est ce qui a refusé la v0.9.130 (#2541 avait
    # déplacé `smart_ai.rs` vers `tune-smart-http`).
    if fautives := caisses_a_routes_non_scannees():
        print("le scanner est aveugle à des routes RÉELLEMENT servies :", file=sys.stderr)
        for caisse, compte in fautives:
            print(f"    {caisse} déclare {compte} route(s), absente de SOURCES_SERVEUR",
                  file=sys.stderr)
        print("Ajouter ces caisses à SOURCES_SERVEUR avant de conclure quoi que ce "
              "soit sur le client web.", file=sys.stderr)
        return 2

    routes = routes_appelees_par_le_web(lire_sources_web(racine_web))
    if not routes:
        print("aucune route extraite du client web — le motif d'appel a changé, "
              "ce contrôle ne garde plus rien", file=sys.stderr)
        return 2

    toutes_absentes = routes_absentes(routes, lire_sources_serveur())

    # Second contrôle, conscient du préfixe. Ce que le premier rate par
    # construction : une route servie sous un AUTRE préfixe, ou un segment qui
    # n'est qu'une sous-chaîne d'un mot du code.
    for chemin, cause in routes_de_prefixe_absentes(routes, REPO_ROOT / "tune-server/src"):
        if chemin not in {r for r, _ in toutes_absentes}:
            toutes_absentes.append((chemin, cause))
    toutes_absentes.sort()

    absentes = [(r, s) for r, s in toutes_absentes if r not in SOCLE_CONNU]
    tolerees = [(r, s) for r, s in toutes_absentes if r in SOCLE_CONNU]

    print(f"routes appelées par le web : {len(routes)}")
    for route, _ in tolerees:
        print(f"  toléré ({SOCLE_CONNU[route]}) : {route}")

    # Une entrée du socle qui a disparu des absentes est une route réparée :
    # le dire, pour que la liste ne fossilise pas une dette déjà payée.
    reparees = sorted(set(SOCLE_CONNU) - {r for r, _ in toutes_absentes})
    for route in reparees:
        print(f"  ✓ {route} est désormais servie — la retirer de SOCLE_CONNU")

    if not absentes:
        print("✓ aucune route absente hors socle connu")
        return 0

    print(f"✗ {len(absentes)} route(s) NOUVELLE(s) appelée(s) mais introuvable(s) côté serveur :")
    for route, segment in absentes:
        print(f"    {route}   (segment « {segment} »)")
    print()
    print("Publier en l'état livrerait ces écrans en appelant des routes absentes.")
    print("Vérifier qu'une PR serveur non fusionnée ne les fournit pas.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
