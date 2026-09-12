#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Detecteur de nouveautes du forum, par MESSAGE et non par etat de fil.

C'est le mecanisme des rondes de tri, versionne ici. Il vit en exploitation
dans `~/.claude/tune-agents/nouveautes-forum.py`, appele par `ronde-tri.sh`
toutes les 600 s. CE FICHIER EST LA REFERENCE : la copie hors depot doit lui
etre identique. Il n'existe pas deux implantations, et il ne doit pas en
exister deux — voir « CE QUI RESTE A BRANCHER » en bas de cet en-tete.


LE DEFAUT QU'IL FERME — issue #2910
-----------------------------------
Le critere des rondes etait, mot pour mot (ancien `prompt-tri.txt`, ligne 3) :

    « relever les fils dont le DERNIER message n'est pas de Bertrand/Admin »

C'est un ETAT DE FIL lu a un instant, pas un inventaire de messages. Il n'a
donc AUCUNE MEMOIRE : tout ce qui arrive et recoit reponse entre deux regards
est perdu pour toujours, et rien ne le signale.

Rejeu du 2026-09-01 sur la fenetre du 28/08 00h00 au 30/08 14h49 (heure de
Paris), avec les instants de ronde REELS releves dans `journaux/tri.log` :

    126 messages, dont 56 de testeurs
    critere retire : 26 vus, 30 RATES  (54 %)

Les trois cas nommes par #2910 sont dans les 30. Ce qui les a ecartes, mesure :

    reponse 5922  Sandro   fil 1579  28/08 16:48
        derniere ronde avant 16:05:55, suivante 17:31:12 — 85 min d'aveugle,
        dont 75 min de ronde EN COURS. L'equipe avait repondu (5923) a 17:05.
    reponse 5949  Dimitri  fil 1577  29/08 12:00
    reponse 5958  Lulu     fil 1591  29/08 14:36
        derniere ronde avant 28/08 17:31:12, suivante 29/08 18:31:01 —
        1500 min (25 h) sans aucune ronde. L'equipe avait repondu (5951, 5960).

A la ronde du 29/08 18:31, apres ces 25 h, le critere retire retenait
248 fils — dont UN SEUL actif dans les 72 h (le 1611). Tout ce qui etait
arrive pendant le trou avait recu sa reponse, et les 247 autres etaient de
vieux fils inexploitables, dont 42 par le seul effet du nom manquant
« Bertrand Clech ». Un arret de la ronde ne retarde donc pas le tri avec ce
critere — il l'efface.

Le raffinement du fil 1591, contre-intuitif et verifie : un fil qui redevient
vivant ne rattrape pas ses messages manques, puisque seul son DERNIER message
est relu. Un fil peut donc etre traite et laisser un trou au milieu.


LES QUATRE FAMILLES, MESUREES SUR LE CORPUS COMPLET (1604 fils, 2026-09-01)
--------------------------------------------------------------------------
1. QUESTION LAISSEE AU-DESSUS. Le critere ne regarde meme pas un fil dont
   l'equipe a parle en dernier. 911 fils sont dans cet etat avec un testeur
   present ; 296 portent au-dessus un message de testeur d'au moins 15 mots
   contenant un « ? ». (296 est un PLAFOND : l'heuristique du point
   d'interrogation ne dit pas si la question a recu reponse.)
2. TESTEUR PRIS POUR UN ADMIN. Non realise aujourd'hui : 120 auteurs
   distincts, ZERO collision exacte avec un nom d'equipe. Mais le risque est
   structurel — `GET /threads/{slug}` ne rend AUCUN identifiant d'auteur
   (cles d'une reponse : author, body, created_at, id), donc l'appartenance a
   l'equipe ne peut se decider que sur un nom affiche. D'ou le garde-fou
   `auditer_equipe()` ci-dessous, qui hurle sur un homonyme approchant.
   A noter, dans l'autre sens : le critere retire ne nommait que
   « Bertrand/Admin », alors que l'equipe compte quatre comptes. 42 fils
   avaient « Bertrand Clech » en dernier message et etaient donc retenus a
   tort, a CHAQUE ronde.
3. EDITION. Le contenu change, la date du dernier message ne bouge pas. Non
   fermable au cout actuel de l'API, et je ne pretends pas le fermer :
   `updated_at` n'existe QUE sur le detail d'un fil (la liste ne le rend pas)
   et il est trop bruyant pour servir de signal — 977 fils sur 1119 ont un
   `updated_at` posterieur de plus de deux minutes a leur dernier message,
   jusqu'a +2632 h. Il n'existe aucun `updated_at` par reponse. Fermer cette
   famille demande un champ d'API, pas un script.
4. FENETRE QUI GLISSE. C'est la famille des trois cas ci-dessus, et c'est
   celle que ce fichier ferme : l'inventaire par message a une memoire, donc
   la duree du trou n'a plus d'effet.


CE QUE FAIT CE SCRIPT
---------------------
  - il inventorie les fils en 8 requetes (`per_page=200`, plafond verifie :
    500 est refuse), soit ~1 s ;
  - il compare `replies_count` fil par fil a l'instantane precedent, sur TOUT
    le corpus — pas sur une fenetre ;
  - il ne relit en detail que les fils dont le compte a bouge, ET les fils
    apparus depuis (correction ci-dessous) ;
  - il rend la liste des MESSAGES dont l'id depasse le filigrane.

Un message reste dans la liste de travail quoi qu'il arrive APRES lui. C'est
tout le point : la liste est faite de messages, pas d'etats de fil.

Invariant sur lequel repose le filigrane, VERIFIE et non suppose : les ids de
reponse sont strictement croissants dans le temps. Zero violation sur les
5559 reponses du corpus au 2026-09-01. Si cet invariant tombe un jour, ce
script rate des messages en silence — le revalider avant d'y toucher.

L'instantane n'avance QUE sur `--valider`, appele apres un passage complet :
un tour rate se rejoue au tour suivant au lieu de disparaitre.


LA CORRECTION APPORTEE ICI, ET SON COUT
---------------------------------------
Un fil NEUF n'exposait que son CORPS. Ses reponses deja presentes au moment
ou on le decouvre n'etaient jamais listees, et l'instantane scellait pourtant
son `replies_count` : si le fil ne rebougeait plus jamais, ces reponses
etaient perdues DEFINITIVEMENT.

Mesure : 248 fils sur 1119 ont recu leur premiere reponse en moins de 600 s —
c'est-a-dire dans l'intervalle d'un seul tour de ronde. Parmi eux, 30 n'ont
plus jamais bouge : 35 reponses, dont 4 de testeurs, etaient donc
irrecuperables. Cas reel, et le pire possible : fil 903, note de version
publiee par « Admin » le 03/07 a 08h40 ; Benjithom y confirme a 08h45:44 que
la lecture refonctionne en 0.8.239. Le fil ne bougera plus. L'ancienne
branche ne rendait RIEN du tout sur ce fil : la ligne de fil est ecartee
(auteur d'equipe) et la reponse n'etait pas lue.

Cout mesure sur la fenetre du 28 au 30/08 : ZERO message ramasse en trop
(aucun fil neuf de la fenetre n'avait de reponse dans les 600 s). Cout en
requetes : une lecture de detail par fil neuf portant au moins une reponse.

Usage :
    nouveautes-forum.py            # detecte, ecrit etat/nouveautes.tsv,
                                   # code 0 s'il y a du neuf de TESTEUR, 1 sinon
    nouveautes-forum.py --valider  # avance l'instantane (passage reussi)
    nouveautes-forum.py --amorcer  # sceller l'etat courant sans rien signaler
    nouveautes-forum.py --self-test  # contre-epreuve hors ligne, aucun reseau


CE QUI RESTE A BRANCHER — hors depot, et pas de mon ressort
-----------------------------------------------------------
Tant que `~/.claude/tune-agents/nouveautes-forum.py` n'a pas ete remplace par
ce fichier, la correction du fil neuf est ECRITE MAIS PAS BRANCHEE. Le
remplacement se fait ronde a l'arret (`pgrep -f ronde-tri.sh`, `ls -d
~/.claude/tune-agents/.verrou-tri`), par copie a cote puis `mv` atomique.
"""

import json
import os
import sys
import time
import unicodedata
import urllib.request

BASE = os.path.dirname(os.path.abspath(__file__))
ETAT = os.path.join(BASE, "etat", "instantane-forum.json")
SORTIE = os.path.join(BASE, "etat", "nouveautes.tsv")
EN_VOL = os.path.join(BASE, "etat", "instantane-forum.encours.json")

API = "https://mozaiklabs.fr/api/v1/forum"
TOKEN = os.environ.get(
    "FORUM_API_TOKEN",
    "5fed36d6029c5c11058925682c77d0a99e49f32c8b0d8d09e96009ba208869cc",
)

# Les quatre comptes d'equipe. Sans « Bertrand Clech » (id 5), un balayage
# declare « jamais repondu » des fils qui l'ont ete — vecu, cf tune-stubs.md.
EQUIPE = frozenset({"Bertrand", "Bertrand Clech", "Admin", "Matteo"})

PER_PAGE = 200          # plafond verifie : 500 est refuse par l'API
TENTATIVES = 3

EN_TETE = ["type", "id", "fil", "slug", "date", "auteur", "camp", "titre_du_fil"]


# ---------------------------------------------------------------- transport

def http(url):
    dernier = None
    for essai in range(TENTATIVES):
        try:
            req = urllib.request.Request(
                url, headers={"Authorization": "Bearer " + TOKEN}
            )
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.loads(r.read().decode("utf-8"))
        except Exception as e:      # reseau, 5xx, JSON tronque
            dernier = e
            time.sleep(1 + essai * 2)
    raise RuntimeError("forum injoignable apres %d essais : %s" % (TENTATIVES, dernier))


def inventaire_http():
    """Tous les fils, en 8 requetes. Rend {id: fil}."""
    fils = {}
    page = 1
    while True:
        d = http("%s/threads?per_page=%d&page=%d" % (API, PER_PAGE, page))
        lot = d.get("threads") or []
        for t in lot:
            fils[t["id"]] = t
        meta = d.get("meta") or {}
        if page >= (meta.get("last_page") or 1) or not lot:
            break
        page += 1
    return fils


def detail_http(fil):
    return http("%s/threads/%s" % (API, fil["slug"]))["thread"]


# ------------------------------------------------------------------ communs

def nom_auteur(o):
    a = o.get("author") or {}
    return a.get("name") if isinstance(a, dict) else a


def camp(auteur):
    return "equipe" if auteur in EQUIPE else "testeur"


def _plie(s):
    """Minuscules, accents replies, espaces normalises — pour l'AUDIT seul."""
    s = unicodedata.normalize("NFKD", str(s or ""))
    s = "".join(c for c in s if not unicodedata.combining(c))
    return " ".join(s.lower().split())


def auditer_equipe(fils, detail_par_fil=None):
    """L'API ne rend aucun identifiant d'auteur : l'appartenance a l'equipe se
    decide sur un NOM AFFICHE, et une egalite exacte. Un homonyme approchant
    (casse, espace, accent) serait donc classe testeur s'il est de l'equipe —
    benin, une ronde depensee — ou classe equipe s'il est testeur, et la son
    message est perdu en silence. On ne devine pas : on signale.

    Rend la liste des noms suspects. ZERO sur le corpus du 2026-09-01
    (120 auteurs distincts)."""
    plies = {_plie(e): e for e in EQUIPE}
    vus = set()
    for t in fils.values():
        vus.add(nom_auteur(t))
    for d in (detail_par_fil or {}).values():
        for r in d.get("replies") or []:
            vus.add(nom_auteur(r))
    suspects = []
    for a in vus:
        if a is None or a in EQUIPE:
            continue
        if _plie(a) in plies:
            suspects.append((a, plies[_plie(a)]))
    return suspects


def ecrire(chemin, obj):
    os.makedirs(os.path.dirname(chemin), exist_ok=True)
    tmp = chemin + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False)
    os.replace(tmp, chemin)


def charger_etat():
    if not os.path.exists(ETAT):
        return None
    with open(ETAT, encoding="utf-8") as f:
        return json.load(f)


def instantane_courant(fils, filigrane):
    return {
        "filigrane": filigrane,
        "comptes": {str(i): t["replies_count"] for i, t in fils.items()},
        "date": time.strftime("%F %T"),
    }


# ------------------------------------------------------------ le mecanisme

def detecter(fils, lire_detail, etat, reponses_des_fils_neufs=True):
    """PUR — aucun reseau, aucun fichier. C'est ici que vit la regle.

    fils        : {id -> fil} tel que rendu par `GET /threads`
    lire_detail : fil -> detail (`GET /threads/{slug}`)
    etat        : instantane precedent, ou None
    reponses_des_fils_neufs : la correction de ce fichier. A False, on
                  retrouve EXACTEMENT le comportement d'avant — un fil neuf
                  n'expose que son corps. `--self-test` s'en sert pour le
                  cote ROUGE ; l'exploitation ne le passe jamais a False.

    Rend (lignes, instantane, details_lus).
    """
    filigrane = (etat or {}).get("filigrane", 0)
    anciens = (etat or {}).get("comptes", {})

    bouges, nouveaux = [], []
    for i, t in fils.items():
        avant = anciens.get(str(i))
        if avant is None:
            nouveaux.append(t)
        elif avant != t["replies_count"]:
            bouges.append(t)

    lignes = []
    details = {}
    nouveau_filigrane = filigrane

    def verser_reponses(t):
        """Toutes les reponses du fil dont l'id depasse le filigrane."""
        nonlocal nouveau_filigrane
        d = lire_detail(t)
        details[str(t["id"])] = d
        for r in d.get("replies") or []:
            if r["id"] <= filigrane:
                continue
            nouveau_filigrane = max(nouveau_filigrane, r["id"])
            auteur = nom_auteur(r)
            lignes.append(
                ["reponse", str(r["id"]), str(t["id"]), t["slug"], r["created_at"],
                 str(auteur), camp(auteur), t["title"]]
            )

    # Un fil neuf porte le message du testeur dans son CORPS, pas dans une
    # reponse : sans cette branche, tout signalement inaugural serait perdu.
    for t in nouveaux:
        auteur = nom_auteur(t)
        if auteur not in EQUIPE:
            lignes.append(
                ["fil", str(t["id"]), str(t["id"]), t["slug"], t["created_at"],
                 str(auteur), "testeur", t["title"]]
            )
        # ... ET ses reponses deja presentes. Sans cette ligne, un fil decouvert
        # apres coup fige son `replies_count` dans l'instantane et ses reponses
        # ne reapparaissent JAMAIS s'il ne rebouge pas. Cas reel : fil 903,
        # reponse 4052 de Benjithom, 5 min 20 s apres une note de version.
        if reponses_des_fils_neufs and t.get("replies_count"):
            verser_reponses(t)

    for t in bouges:
        verser_reponses(t)

    lignes.sort(key=lambda l: int(l[1]))
    instantane = instantane_courant(fils, max(nouveau_filigrane, filigrane))
    return lignes, instantane, details


def critere_retire(fils, lire_detail, equipe=frozenset({"Bertrand", "Admin"})):
    """LE CRITERE RETIRE LE 30/08/2026, reimplante ICI ET NULLE PART AILLEURS,
    pour la seule contre-epreuve : « relever les fils dont le DERNIER message
    n'est pas de Bertrand/Admin ». Il n'est appele que par `--self-test`.

    Rend la liste des (id de fil, type du dernier message, id de ce message,
    auteur de ce message)."""
    retenus = []
    for i, t in sorted(fils.items()):
        dernier = ("fil", t["id"], nom_auteur(t))
        if t.get("replies_count"):
            reps = lire_detail(t).get("replies") or []
            if reps:
                r = max(reps, key=lambda x: x["id"])
                dernier = ("reponse", r["id"], nom_auteur(r))
        if dernier[2] not in equipe:
            retenus.append((t["id"], dernier[0], dernier[1], dernier[2]))
    return retenus


# --------------------------------------------------------------- rejeu hors ligne

def source_fixture(fils_fixture, instant):
    """Rend (fils, lire_detail) tels que l'API les aurait rendus a `instant`.

    Les dates sont comparees en ISO-8601 sur des chaines de meme decalage
    horaire (+02:00 partout dans la fixture) : la comparaison lexicographique
    est alors exactement la comparaison chronologique."""
    fils = {}
    detail = {}
    for f in fils_fixture:
        if f["created_at"] > instant:
            continue
        reps = [r for r in f["replies"] if r["created_at"] <= instant]
        reps.sort(key=lambda r: r["id"])
        fils[f["id"]] = {
            "id": f["id"], "slug": f["slug"], "title": f["title"],
            "author": f["author"], "created_at": f["created_at"],
            "body": f.get("extrait", ""), "replies_count": len(reps),
        }
        detail[f["id"]] = dict(fils[f["id"]], replies=reps)
    return fils, (lambda t: detail[t["id"]])


def _ok(libelle, condition, echecs):
    print("  %-72s %s" % (libelle, "OK" if condition else "ECHEC"))
    if not condition:
        echecs.append(libelle)
    return condition


def self_test():
    """Contre-epreuve ROUGE puis VERT sur les DONNEES REELLES des 48 h.

    Aucun reseau, aucune ecriture : la fixture est un extrait relu par API le
    2026-09-01 et fige dans `nouveautes-forum-fenetre-48h.json`."""
    chemin = os.path.join(BASE, "nouveautes-forum-fenetre-48h.json")
    with open(chemin, encoding="utf-8") as f:
        fx = json.load(f)
    echecs = []

    # ---------------- SCENARIO A : la fenetre du 28 au 30/08 -----------------
    a = fx["scenario_a"]
    instants = a["instants"]
    rates = set(a["rates"])
    temoin = a["temoin"]
    controle = a["controle_positif"]

    print("SCENARIO A — %s" % a["quoi"])
    print("  instants de ronde : %s" % ", ".join(instants))

    # ROUGE : le critere retire, applique a chacun des trois instants reels.
    vus_rouge = set()
    fils_retenus_rouge = set()
    for inst in instants:
        fils, lire = source_fixture(a["fils"], inst)
        for fil_id, typ, msg_id, auteur in critere_retire(fils, lire):
            fils_retenus_rouge.add(fil_id)
            if typ == "reponse":
                vus_rouge.add(msg_id)
    print("  [ROUGE — critere retire, dernier message non-admin]")
    for r in sorted(rates):
        _ok("reponse %d JAMAIS vue (c'est le defaut de #2910)" % r,
            r not in vus_rouge, echecs)
    _ok("controle positif : reponse %d, elle, EST vue" % controle,
        controle in vus_rouge, echecs)
    _ok("temoin : le fil %d n'est PAS retenu" % temoin,
        temoin not in fils_retenus_rouge, echecs)

    # VERT : l'inventaire par message, les memes instants, la memoire en plus.
    # On scelle l'etat au premier instant (comme `--amorcer`), puis on avance.
    fils0, lire0 = source_fixture(a["fils"], instants[0])
    filigrane = 0
    for t in fils0.values():
        if t["replies_count"]:
            for r in lire0(t)["replies"]:
                filigrane = max(filigrane, r["id"])
    etat = instantane_courant(fils0, filigrane)

    vus_vert, fils_vus_vert, contexte = set(), set(), set()
    for inst in instants[1:]:
        fils, lire = source_fixture(a["fils"], inst)
        lignes, etat, _ = detecter(fils, lire, etat)
        for l in lignes:
            if l[6] == "testeur":
                vus_vert.add(int(l[1]))
                fils_vus_vert.add(int(l[2]))
            else:
                contexte.add(int(l[1]))
    print("  [VERT — inventaire par message]")
    for r in sorted(rates):
        _ok("reponse %d selectionnee, camp=testeur" % r, r in vus_vert, echecs)
    _ok("controle positif : reponse %d selectionnee elle aussi" % controle,
        controle in vus_vert, echecs)
    _ok("temoin : le fil %d n'est TOUJOURS pas selectionne" % temoin,
        temoin not in fils_vus_vert, echecs)
    # La reponse 5951 est celle de Bertrand a Dimitri, 1 h 34 apres 5949 :
    # c'est elle qui effacait 5949 pour le critere retire. Elle doit etre du
    # CONTEXTE, et jamais de la matiere — sinon la ronde relit ce qu'elle
    # vient d'ecrire elle-meme.
    _ok("la reponse d'equipe 5951 est du CONTEXTE, jamais de la matiere",
        5951 in contexte and 5951 not in vus_vert, echecs)
    _ok("aucun message d'equipe n'est de la MATIERE (camp=equipe seulement)",
        contexte and not (contexte & vus_vert), echecs)
    print("  les deux chiffres du scenario A — recuperes : %d ; ramasses en trop : %d"
          % (len(vus_vert - vus_rouge), len(fils_vus_vert & {temoin})))

    # ---------------- SCENARIO B : le fil neuf porteur de reponses ----------
    # Meme fonction, meme etat de depart : SEUL `reponses_des_fils_neufs`
    # change entre le rouge et le vert. Le temoin est le fil 750 — neuf lui
    # aussi, porteur d'une reponse dans les 600 s, mais de l'equipe : il doit
    # rester hors de la matiere des deux cotes.
    b = fx["scenario_b"]
    print("SCENARIO B — %s" % b["quoi"])
    fils_av, _ = source_fixture(b["fils"], b["instants"][0])
    fils_ap, lire_ap = source_fixture(b["fils"], b["instants"][1])
    etat_b = instantane_courant(fils_av, 0)

    def lot(vert):
        lignes, etat, _ = detecter(fils_ap, lire_ap, etat_b,
                                   reponses_des_fils_neufs=vert)
        return ({int(l[1]) for l in lignes if l[6] == "testeur"},
                {int(l[1]) for l in lignes if l[6] == "equipe"},
                {int(l[2]) for l in lignes if l[6] == "testeur"},
                etat)

    mat_r, ctx_r, filsmat_r, _ = lot(False)
    mat_v, ctx_v, filsmat_v, etat_v = lot(True)

    print("  [ROUGE — fil neuf : le corps seul]")
    _ok("reponse %d absente de la matiere" % b["rate"], b["rate"] not in mat_r, echecs)
    _ok("controle positif : le fil %d, lui, est bien matiere" % b["controle_positif"],
        b["controle_positif"] in mat_r, echecs)
    _ok("temoin : le fil %d n'est pas matiere" % b["temoin"],
        b["temoin"] not in filsmat_r, echecs)

    print("  [VERT — fil neuf : le corps ET ses reponses]")
    _ok("reponse %d selectionnee, camp=testeur" % b["rate"], b["rate"] in mat_v, echecs)
    _ok("controle positif : le fil %d reste matiere" % b["controle_positif"],
        b["controle_positif"] in mat_v, echecs)
    _ok("temoin : le fil %d n'est TOUJOURS pas matiere" % b["temoin"],
        b["temoin"] not in filsmat_v, echecs)
    _ok("la reponse d'equipe du temoin est versee en CONTEXTE, pas en matiere",
        3610 in ctx_v and 3610 not in mat_v, echecs)
    _ok("le filigrane a avance jusqu'a la reponse %d" % b["rate"],
        etat_v["filigrane"] >= b["rate"], echecs)
    print("  les deux chiffres du scenario B — recuperes : %d ; matiere en trop : %d"
          " (lignes de contexte en plus : %d)"
          % (len(mat_v - mat_r), len(mat_v - mat_r - {b["rate"]}), len(ctx_v - ctx_r)))

    # ---------------- Garde-fou d'homonymie ---------------------------------
    print("GARDE-FOU — homonyme approchant d'un nom d'equipe")
    fils_t, lire_t = source_fixture(a["fils"], instants[-1])
    det_t = {str(i): lire_t(t) for i, t in fils_t.items()}
    _ok("aucun suspect sur la fixture", not auditer_equipe(fils_t, det_t), echecs)
    faux = dict(fils_t)
    faux[999999] = {"id": 999999, "slug": "x", "title": "x", "author": "bertrand",
                    "created_at": instants[-1], "body": "", "replies_count": 0}
    _ok("un « bertrand » minuscule est signale, pas avale",
        [s for s in auditer_equipe(faux, det_t) if s[0] == "bertrand"], echecs)

    print()
    if echecs:
        print("%d ECHEC(S) : %s" % (len(echecs), " | ".join(echecs)))
        return 1
    print("contre-epreuve complete : ROUGE puis VERT, temoin inchange des deux cotes.")
    return 0


# ------------------------------------------------------------------- main

def main():
    args = set(sys.argv[1:])

    if "--self-test" in args:
        return self_test()

    # --valider : le passage s'est bien termine, on scelle ce qui etait en vol.
    if "--valider" in args:
        if not os.path.exists(EN_VOL):
            print("rien en vol — instantane inchange")
            return 0
        os.replace(EN_VOL, ETAT)
        print("instantane avance")
        return 0

    fils = inventaire_http()
    etat = charger_etat()

    if etat is None or "--amorcer" in args:
        # Amorcage : on scelle l'etat courant SANS rien signaler, sinon le
        # premier passage deverserait tout le corpus d'un coup. Il faut lire le
        # detail de TOUS les fils qui ont des reponses — environ 1100 requetes,
        # deux minutes, une seule fois. Ne PAS se limiter aux fils les plus
        # recemment crees : le message le plus recent du forum est souvent sur
        # un vieux fil. Un amorcage tronque poserait un filigrane trop bas et
        # ferait rejouer des messages deja traites.
        filigrane = 0
        for t in fils.values():
            if not t["replies_count"]:
                continue
            for r in detail_http(t).get("replies") or []:
                filigrane = max(filigrane, r["id"])
        ecrire(ETAT, instantane_courant(fils, filigrane))
        open(SORTIE, "w").close()
        print("amorce : %d fils scelles, filigrane = reponse %d" % (len(fils), filigrane))
        return 1

    lignes, instantane, details = detecter(fils, detail_http, etat)

    os.makedirs(os.path.dirname(SORTIE), exist_ok=True)
    with open(SORTIE, "w", encoding="utf-8") as f:
        f.write("\t".join(EN_TETE) + "\n")
        for l in lignes:
            f.write("\t".join(x.replace("\t", " ").replace("\n", " ") for x in l) + "\n")

    # L'instantane part « en vol » : il ne remplacera l'etat que sur --valider.
    ecrire(EN_VOL, instantane)

    for a, e in auditer_equipe(fils, details):
        print("ATTENTION homonyme : %r ressemble a %r — verifier le compte avant"
              " de conclure sur son camp" % (a, e), file=sys.stderr)

    # L'age de l'instantane precedent : un trou de ronde ne fait plus perdre de
    # message, mais il reste une information (25 h de silence le 28-29/08).
    age = ""
    if etat.get("date"):
        try:
            ecart = time.time() - time.mktime(time.strptime(etat["date"], "%Y-%m-%d %H:%M:%S"))
            age = " | instantane precedent vieux de %.1f h" % (ecart / 3600.0)
        except ValueError:
            pass

    testeurs = [l for l in lignes if l[6] == "testeur"]
    print("fils %d | messages neufs %d (dont testeurs %d)%s"
          % (len(fils), len(lignes), len(testeurs), age))

    # On ne reveille un agent que s'il y a de la matiere de TESTEUR : une
    # reponse de l'equipe faisait bouger l'empreinte et depensait une ronde
    # entiere pour relire ce qu'on venait d'ecrire soi-meme.
    return 0 if testeurs else 1


if __name__ == "__main__":
    sys.exit(main())
