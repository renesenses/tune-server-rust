#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Verse la matière d'un message du forum dans une issue — UNE SEULE FOIS.

Défaut corrigé (issue #2919, mesuré le 30/08/2026)
--------------------------------------------------

Les rondes de tri recalculaient à chaque passage, depuis le forum, l'ensemble
des messages à traiter, et **rien dans le ticket ne disait qu'un message y
avait déjà été versé**. Résultat mesuré sur le corpus complet du dépôt :
**15 issues** portent une citation de testeur déposée deux ou trois fois, pour
**24 dépôts redondants** (relevé par API le 2026-09-01, même critère que
l'issue : ligne de citation `>` d'au moins 12 mots, comparée sur ses 14
premiers mots normalisés).

La cause n'est pas le détecteur de nouveautés — il rend chaque message une
fois. C'est le **rejeu** : une ronde qui a déjà posté ses commentaires puis
sort en erreur ne fait pas avancer son instantané, et le tour suivant rejoue
le même lot. Sans mémoire côté ticket, le rejeu redépose.

Le contrôle existait en consigne (« relis les commentaires avant d'écrire »).
Une consigne n'est pas un mécanisme : elle tient tant que l'agent la lit. Ce
script est le mécanisme. Il est le SEUL chemin de versement : il lit, décide,
et pose lui-même le marqueur — l'agent ne peut plus l'oublier.

La clef d'idempotence
---------------------

    forum-reply:<id de la réponse>      pour une réponse de fil
    forum-thread:<id du fil>            pour le corps d'un fil neuf

posée **seule sur la dernière ligne** du commentaire, et cherchée par
**égalité exacte** sur une ligne entière — jamais en sous-chaîne.

Pourquoi l'id de réponse, et pas autre chose :

- l'horodatage n'est pas stable (le forum rend `created_at` à la seconde, et
  deux messages d'un même testeur peuvent partager la minute) ;
- le numéro de FIL ne mord pas : dans #693, les deux dépôts du même message le
  rattachaient à deux fils **différents**, 1106 puis 1113 ;
- un extrait de texte se reformule d'un passage à l'autre ;
- l'id de réponse est la clef primaire du forum. Vérifié le 2026-09-01 sur le
  corpus complet : **5559 réponses, 5559 ids distincts** — donc
  **5559 clefs distinctes pour 5559 messages, zéro collision**.

Pourquoi l'égalité de ligne entière, et pas un `grep` nu — c'est le piège
mesuré, et il coûte des messages avalés en silence :

- **552 des 5559 ids (9,9 %) sont le préfixe décimal d'un autre id**
  (86 ids à deux chiffres, 895 à trois, pour un maximum de 6034). Un
  `grep -c "forum-reply:602"` rend 2 sur une issue marquée `forum-reply:6029`,
  alors que la réponse 602 est un vrai message de testeur. Sous cette forme,
  552 messages seraient jetés comme « déjà déposés » — un par un, sans trace.
  Côté fils, 138 des 1604 ids sont dans le même cas.
- le marqueur **nommé en prose** compterait aussi. Un agent qui écrit
  « aucun commentaire ne porte forum-reply:6029 » puis renonce rendrait le
  message indéposable pour toujours.

L'ancrage `^…$` ferme les deux, et l'égalité exacte de l'id ferme le premier
même si quelqu'un relâche l'ancrage un jour.

Usage
-----

    verser-message-forum.py --issue 693 --reponse 5949 --corps commentaire.md
    verser-message-forum.py --issue 2551 --fil 1620 --corps commentaire.md
    verser-message-forum.py --issue 693 --reponse 5949 --verifier
    verser-message-forum.py --self-test

Codes de sortie :
    0  versé (ou, avec --verifier, jamais versé jusqu'ici)
    3  DÉJÀ VERSÉ — rien n'a été posté, et c'est un succès, pas une panne
    2  erreur (arguments, GitHub injoignable)
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile

DEPOT_PAR_DEFAUT = "renesenses/tune-server-rust"

# Le marqueur est SEUL sur sa ligne. Voir l'en-tête : sans l'ancrage, 552 ids
# de réponse sur 5559 sont avalés par le préfixe d'un id plus long.
MOTIF_MARQUEUR = re.compile(r"^[ \t]*forum-(reply|thread):([0-9]+)[ \t]*$", re.M)


def clef(genre, ident):
    """Clef d'idempotence d'un message forum. `genre` vaut reply ou thread."""
    if genre not in ("reply", "thread"):
        raise ValueError("genre inconnu : %r" % genre)
    return "forum-%s:%d" % (genre, int(ident))


def marqueurs(texte):
    """Ensemble des clefs réellement POSÉES dans un texte.

    On rend des clefs normalisées (`forum-reply:5949`), jamais des positions :
    la comparaison qui suit est une appartenance à un ensemble, donc une
    égalité exacte. Un id plus court ne peut pas y entrer par la bande.
    """
    return {
        "forum-%s:%d" % (g, int(i))
        for g, i in MOTIF_MARQUEUR.findall(texte or "")
    }


def deja_verse(textes, cle):
    """Vrai si la clef est déjà posée dans l'un des textes (corps + commentaires)."""
    for t in textes:
        if cle in marqueurs(t):
            return True
    return False


# ---------------------------------------------------------------- GitHub


def _gh(args, entree=None):
    r = subprocess.run(["gh"] + args, capture_output=True, text=True, input=entree)
    if r.returncode != 0:
        raise RuntimeError("gh %s : %s" % (" ".join(args), r.stderr.strip()[:400]))
    return r.stdout


def _documents(txt):
    """`gh api --paginate` colle les pages : plusieurs documents JSON à la suite."""
    out, dec, i = [], json.JSONDecoder(), 0
    txt = txt.strip()
    while i < len(txt):
        while i < len(txt) and txt[i].isspace():
            i += 1
        if i >= len(txt):
            break
        obj, i = dec.raw_decode(txt, i)
        out.extend(obj if isinstance(obj, list) else [obj])
    return out


def corpus_issue(depot, numero):
    """Corps de l'issue + TOUS ses commentaires.

    La pagination est obligatoire : `gh issue view --json comments` plafonne, et
    c'est justement dans les issues les plus commentées — donc les plus exposées
    au double dépôt — que le plafond mord.
    """
    issue = _documents(_gh(["api", "repos/%s/issues/%d" % (depot, numero)]))
    textes = [(issue[0].get("body") or "") if issue else ""]
    for c in _documents(_gh([
        "api", "--paginate",
        "repos/%s/issues/%d/comments?per_page=100" % (depot, numero),
    ])):
        textes.append(c.get("body") or "")
    return textes


def poster(depot, numero, corps):
    """Poste le commentaire. Le corps passe par stdin : ni guillemet ni accent
    ne traverse une ligne de commande."""
    return _documents(_gh(
        ["api", "repos/%s/issues/%d/comments" % (depot, numero), "--input", "-"],
        entree=json.dumps({"body": corps}),
    ))


def verser(depot, numero, genre, ident, corps, simuler=False):
    """Chemin unique de versement. Rend le code de sortie."""
    cle = clef(genre, ident)
    if deja_verse(corpus_issue(depot, numero), cle):
        print("DEJA VERSE — %s porte deja %s : rien n'est poste." % (
            "#%d" % numero, cle))
        return 3
    # Le marqueur est posé par le script, jamais par l'appelant : un marqueur
    # oublié est un dépôt en double au tour suivant.
    corps = corps.rstrip() + "\n\n" + cle + "\n"
    if simuler:
        print("SIMULATION — aurait poste sur #%d avec %s" % (numero, cle))
        return 0
    r = poster(depot, numero, corps)
    print("verse sur #%d (%s) : %s" % (
        numero, cle, r[0].get("html_url") if r else "?"))
    return 0


# ---------------------------------------------------------------- contre-épreuve


FAUX_GH = r'''#!/usr/bin/env python3
"""gh factice : sert un magasin JSON et enregistre les commentaires postes."""
import json, os, sys

ETAT = os.environ["FAUX_GH_ETAT"]
etat = json.load(open(ETAT))
a = sys.argv[1:]
if a[0] != "api":
    sys.exit("gh factice : sous-commande inattendue %r" % a[0])
chemin = [x for x in a[1:] if not x.startswith("-")]
chemin = chemin[0].split("?")[0] if chemin else ""
if "--input" in a:
    corps = json.loads(sys.stdin.read())["body"]
    etat["commentaires"].append(corps)
    json.dump(etat, open(ETAT, "w"))
    print(json.dumps({"html_url": "https://exemple/%d" % len(etat["commentaires"])}))
elif chemin.endswith("/comments"):
    print(json.dumps([{"body": b} for b in etat["commentaires"]]))
else:
    print(json.dumps({"body": etat["corps"]}))
'''


def _fabrique_faux_gh(rep):
    chemin = os.path.join(rep, "gh")
    with open(chemin, "w", encoding="utf-8") as f:
        f.write(FAUX_GH)
    os.chmod(chemin, 0o755)
    return chemin


CITATION = (
    "> Je confirme donc, avec un petit delai, que tout est rentre dans "
    "l'ordre du cote du scrobbling Last.fm (teste ce jour sur la 0.9.125)"
)
TEMOIN = (
    "> Le renderer DLNA reste fige sur le nom de la station et ne suit pas "
    "le titre en cours, meme apres un changement de piste"
)


def _compter(etat, fragment):
    return sum(1 for c in etat["commentaires"] if fragment in c)


def _ronde(env, rep, numero, genre, ident, corps, controle):
    """Un passage de ronde. `controle=False` reproduit la ronde d'AVANT #2919 :
    elle poste sans jamais regarder ce qui est déjà là."""
    if not controle:
        etat = json.load(open(env["FAUX_GH_ETAT"]))
        etat["commentaires"].append(corps)
        json.dump(etat, open(env["FAUX_GH_ETAT"], "w"))
        return 0
    fichier = os.path.join(rep, "corps.md")
    with open(fichier, "w", encoding="utf-8") as f:
        f.write(corps)
    r = subprocess.run(
        [sys.executable, os.path.abspath(__file__),
         "--issue", str(numero), "--" + ("reponse" if genre == "reply" else "fil"),
         str(ident), "--corps", fichier, "--depot", "essai/essai"],
        capture_output=True, text=True, env=env)
    return r.returncode


def _magasin(rep, corps="Corps de l'issue.", commentaires=None):
    chemin = os.path.join(rep, "magasin.json")
    with open(chemin, "w", encoding="utf-8") as f:
        json.dump({"corps": corps, "commentaires": list(commentaires or [])}, f)
    env = dict(os.environ)
    env["PATH"] = rep + os.pathsep + env["PATH"]
    env["FAUX_GH_ETAT"] = chemin
    return chemin, env


def self_test():
    echecs = []

    def verifier(nom, condition, detail=""):
        print("  %-58s %s" % (nom, "OK" if condition else "ECHEC " + detail))
        if not condition:
            echecs.append(nom)

    # 1. Deux rondes de suite, une fois SANS la clef (la ronde d'avant #2919),
    #    une fois AVEC. Le scénario est celui qui a produit les 24 dépôts
    #    redondants : la ronde 1 verse, elle sort en erreur, l'instantané
    #    n'avance pas, et la ronde 2 rejoue le même lot — augmenté du message
    #    arrivé entre-temps.
    #
    #    TÉMOIN : la réponse 5975, message distinct apparu à la ronde 2. Elle
    #    doit être versée exactement une fois DES DEUX CÔTÉS. C'est ce qui
    #    montre que la clef bloque le rejeu sans rien avaler d'autre.
    for controle in (False, True):
        etiquette = "AVEC clef" if controle else "SANS clef (ronde d'avant)"
        with tempfile.TemporaryDirectory(suffix="-p2a-2919") as rep:
            _fabrique_faux_gh(rep)
            chemin, env = _magasin(rep)
            corps_a = "## Ronde de tri\n\n" + CITATION + "\n"
            corps_t = "## Ronde de tri\n\n" + TEMOIN + "\n"
            # Ronde 1 : le message 5949.
            _ronde(env, rep, 693, "reply", 5949, corps_a, controle)
            # Ronde 2 : REJEU à l'identique de 5949, plus le témoin 5975.
            code_rejeu = _ronde(env, rep, 693, "reply", 5949, corps_a, controle)
            code_temoin = _ronde(env, rep, 693, "reply", 5975, corps_t, controle)
            etat = json.load(open(chemin))
            n_cit = _compter(etat, CITATION)
            n_tem = _compter(etat, TEMOIN)
            attendu = 1 if controle else 2
            verifier("[%s] citation rejouee presente %d fois" % (etiquette, attendu),
                     n_cit == attendu, "attendu %d, obtenu %d" % (attendu, n_cit))
            verifier("[%s] TEMOIN (message distinct) present 1 fois" % etiquette,
                     n_tem == 1, "attendu 1, obtenu %d" % n_tem)
            if controle:
                verifier("[%s] le rejeu sort en code 3" % etiquette,
                         code_rejeu == 3, "code %d" % code_rejeu)
                verifier("[%s] le temoin, lui, sort en code 0" % etiquette,
                         code_temoin == 0, "code %d" % code_temoin)
                verifier("[%s] le marqueur est pose sur chaque depot" % etiquette,
                         all("forum-reply:" in c for c in etat["commentaires"]))

    # 2. Deux messages distincts produisent deux clefs distinctes.
    verifier("clefs distinctes pour deux messages distincts",
             clef("reply", 5949) != clef("reply", 5975))
    verifier("les 200 premiers ids donnent 200 clefs",
             len({clef("reply", i) for i in range(1, 201)}) == 200)
    verifier("reponse et fil ne partagent pas d'espace de clefs",
             clef("reply", 1113) != clef("thread", 1113))

    # 3. Le piège du préfixe : 552 ids sur 5559 sont le préfixe d'un autre.
    #    Un contrôle en sous-chaîne les avalerait ; celui-ci ne doit pas.
    deja = "## Ronde de tri\n\n> extrait\n\nforum-reply:6029\n"
    verifier("un id prefixe (602) n'est PAS avale par 6029",
             not deja_verse([deja], clef("reply", 602)))
    verifier("le controle naif, lui, l'avalerait — c'est bien le piege",
             "forum-reply:602" in deja)
    verifier("6029 lui-meme est bien reconnu",
             deja_verse([deja], clef("reply", 6029)))

    # 4. Un marqueur NOMMÉ EN PROSE n'est pas un marqueur posé.
    prose = "Aucun commentaire ne porte forum-reply:6029, je verse donc.\n"
    verifier("marqueur cite en prose : ne compte pas",
             not deja_verse([prose], clef("reply", 6029)))

    # 5. Le corps de l'issue compte autant qu'un commentaire : une issue creee
    #    par la ronde porte son marqueur dans son CORPS.
    verifier("marqueur dans le corps de l'issue : compte",
             deja_verse(["Signalement.\n\nforum-thread:1620\n", "autre"],
                        clef("thread", 1620)))

    # 6. Le versement en simulation ne poste rien.
    with tempfile.TemporaryDirectory(suffix="-p2a-2919") as rep:
        _fabrique_faux_gh(rep)
        chemin, env = _magasin(rep)
        os.environ["PATH"] = env["PATH"]
        os.environ["FAUX_GH_ETAT"] = chemin
        try:
            code = verser("essai/essai", 42, "reply", 5949, "corps", simuler=True)
        finally:
            os.environ["PATH"] = os.environ["PATH"].split(os.pathsep, 1)[1]
        verifier("--simuler ne poste rien",
                 code == 0 and not json.load(open(chemin))["commentaires"])

    if echecs:
        print("verser-message-forum self-test: %d ECHEC(S)" % len(echecs))
        return 1
    print("verser-message-forum self-test: PASS")
    return 0


# ---------------------------------------------------------------- CLI


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--issue", type=int)
    ap.add_argument("--reponse", type=int, help="id de la reponse forum versee")
    ap.add_argument("--fil", type=int, help="id du fil, pour un signalement inaugural")
    ap.add_argument("--corps", help="fichier contenant le commentaire a poster")
    ap.add_argument("--depot", default=DEPOT_PAR_DEFAUT)
    ap.add_argument("--verifier", action="store_true",
                    help="ne poste rien : dit seulement si le message est deja verse")
    ap.add_argument("--simuler", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()

    if a.self_test:
        return self_test()

    if a.issue is None or (a.reponse is None) == (a.fil is None):
        ap.error("--issue et exactement l'un de --reponse / --fil sont requis")
    genre, ident = ("reply", a.reponse) if a.reponse is not None else ("thread", a.fil)

    if a.verifier:
        cle = clef(genre, ident)
        if deja_verse(corpus_issue(a.depot, a.issue), cle):
            print("DEJA VERSE — #%d porte deja %s" % (a.issue, cle))
            return 3
        print("jamais verse — #%d ne porte pas %s" % (a.issue, cle))
        return 0

    if not a.corps:
        ap.error("--corps est requis pour verser")
    with open(a.corps, encoding="utf-8") as f:
        corps = f.read()
    if not corps.strip():
        ap.error("corps vide : rien a verser")
    return verser(a.depot, a.issue, genre, ident, corps, simuler=a.simuler)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as e:
        print("erreur : %s" % e, file=sys.stderr)
        sys.exit(2)
