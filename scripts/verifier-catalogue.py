"""Contrôle du vocabulaire du catalogue dans les fichiers suivis par Git."""
from functools import lru_cache
import base64
import binascii
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import unicodedata

CLE = os.environ.get("CATALOGUE_HMAC_KEY", "").encode()

PROFILS = {
    "41e64c299b7745150eb5ee118fcca9bdcde9af5cc631ca998bc8696b3bc6761d",
    "4d4735fac93410ecc4c2f6501991fde9edad64062d202cc6f3f1ccbf516c94c5",
    "79c585c00ce21d16fa0d4b257e618941cbda0f4f2db597a4b60bb4179ef6d7c2",
}
AXES = {
    "8e5f86b2c3f1fd601339cac0600875564b8d3c06fa522d6ffca8a4cb1790da7f",
    "60fa1feb41f9e2a746f50c8cb758228699bb09bc65076e961a9ba5803a7d1cfa",
    "794e80a5d906e1c14c774d540c1eebaba585eb252351b235d8bf53ba4a8f35b2",
    "6cff2d1108127223c370a5c3668261cfe95cd9486acf2ec0a2ff89e33c808165",
}

DOSSIER = Path(__file__).resolve().parent / "catalogue"
CANARI = (DOSSIER / "canari.sha256").read_text().strip()
# Sous-ensemble ASCII alphanumérique de confusables.txt, Unicode 16.0.0.
# Source : https://www.unicode.org/Public/security/16.0.0/confusables.txt
CONFUSABLES = str.maketrans(json.loads((DOSSIER / "confusables.json").read_text()))
VERS_ASCII = {k: v for k, v in CONFUSABLES.items() if k > 127}
MOTS = re.compile(r"[^\W_]+(?:[-_][^\W_]+)*")
ENCODE = re.compile(r"[A-Za-z0-9+/_-]{8,}={0,2}")
HEX = re.compile(r"(?:[a-fA-F0-9]{2}){4,}")
# Les longueurs publiques sont celles des identifiants déclarés. Pour un autre
# catalogue injecté par un appelant, aucune supposition sur ses longueurs.
CATALOGUE_DU_DEPOT = frozenset(PROFILS | AXES)
TAILLES_DU_DEPOT = (5, 6, 7, 9)


@lru_cache(maxsize=65536)
def empreinte(mot):
    return hmac.digest(CLE, mot.encode("utf-8"), "sha256").hex()


def canonique(texte):
    if texte.isascii():
        return texte.lower()
    plie = unicodedata.normalize("NFKC", texte).casefold()
    # Le point combinant de İ appartient au même jeton que sa lettre.
    return "".join(c for c in unicodedata.normalize("NFD", plie) if not unicodedata.combining(c))


def squelette(texte):
    return unicodedata.normalize("NFD", texte).translate(CONFUSABLES).casefold()


@lru_cache(maxsize=32768)
def decodages(bloc):
    octets = []
    if HEX.fullmatch(bloc):
        octets.append(bytes.fromhex(bloc))
    try:
        octets.append(base64.b64decode(bloc + "=" * (-len(bloc) % 4), altchars=b"-_", validate=True))
    except (ValueError, binascii.Error):
        pass
    textes = []
    for contenu in octets:
        try:
            texte = contenu.decode("utf-16" if contenu.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8")
        except UnicodeError:
            continue
        if texte and texte not in textes:
            textes.append(texte)
    return tuple(textes)


class Detecteur:
    def __init__(self):
        self.signatures = frozenset(PROFILS | AXES)
        self.connus = {}
        self.motifs = None
        self.recherche = lru_cache(maxsize=32768)(self._recherche)

    def reconnaitre(self, mot):
        signature = empreinte(mot)
        if signature in self.signatures:
            self.connus[mot] = signature
            return signature
        return None

    def preparer(self, textes):
        # Index privé en mémoire. Chaque candidat est authentifié avant de
        # servir à la recherche accélérée. Les mots absents restent contrôlés.
        for texte in textes:
            for mot in re.findall(r"[a-zA-Z]+", texte):
                if self.reconnaitre(mot.lower()) and len(self.connus) == len(self.signatures):
                    self.indexer()
                    return
        self.indexer()

    def indexer(self):
        self.motifs = {}
        for mot, signature in self.connus.items():
            for forme in (mot, squelette(mot)):
                self.motifs.setdefault(forme, set()).add(signature)
        self.recherche.cache_clear()

    def _recherche(self, mot):
        formes = {mot, squelette(mot)}
        if not mot.isascii():
            formes.add(mot.translate(VERS_ASCII))
        touches = set()
        if self.motifs:
            for forme in formes:
                for cle, signatures in self.motifs.items():
                    pos = forme.find(cle)
                    while pos >= 0:
                        touches.update((pos, s) for s in signatures)
                        pos = forme.find(cle, pos + 1)
        if self.motifs is None or len(self.connus) != len(self.signatures):
            tailles = TAILLES_DU_DEPOT if self.signatures == CATALOGUE_DU_DEPOT else range(1, len(mot) + 1)
            for forme in formes:
                for taille in tailles:
                    for pos in range(len(forme) - taille + 1):
                        signature = empreinte(forme[pos:pos + taille])
                        if signature in self.signatures:
                            touches.add((pos, signature))
        return tuple(sorted(touches))

    def positions(self, texte):
        for token in MOTS.finditer(canonique(texte.replace("\x00", ""))):
            mot = token.group().replace("-", "").replace("_", "")
            for pos, signature in self.recherche(mot):
                yield token.start(), pos, signature

    def ligne(self, ligne):
        trouves = [(0, 0, *t) for t in self.positions(ligne)]
        for bloc in ENCODE.finditer(ligne):
            for indice, variante in enumerate(decodages(bloc.group()), 1):
                trouves.extend((indice, bloc.start(), *t) for t in self.positions(variante))
        return sorted(set(trouves))


def infractions(texte):
    # Point d’entrée du banc indépendant, sans lecture du dépôt.
    detecteur = Detecteur()
    for numero, ligne in enumerate(texte.splitlines(), 1):
        for *_, signature in detecteur.ligne(ligne):
            yield numero, signature


def texte_du_fichier(contenu):
    if contenu.startswith((b"\xff\xfe", b"\xfe\xff")):
        return contenu.decode("utf-16", errors="surrogatepass")
    return contenu.decode("utf-8", errors="surrogateescape")


def revisions(evenement, racine):
    pr = evenement.get("pull_request", {})
    base = pr.get("base", {}).get("sha") or evenement.get("before")
    tete = pr.get("head", {}).get("sha") or evenement.get("after") or os.environ.get("GITHUB_SHA")
    if not tete:
        return []
    if not re.fullmatch(r"[0-9a-f]{40}", tete) or (base and not re.fullmatch(r"[0-9a-f]{40}", base)):
        raise ValueError("Référence Git invalide")
    if not base or base == "0" * 40:
        branche = evenement["repository"]["default_branch"]
        ref = "refs/remotes/origin/" + branche
        subprocess.run(["git", "check-ref-format", ref], check=True)
        base = subprocess.check_output(["git", "-C", racine, "merge-base", ref, tete], text=True).strip()
    if base == tete:
        return [tete]
    return subprocess.check_output(["git", "-C", racine, "rev-list", base + ".." + tete], text=True).splitlines()


def charger_liste(contenu):
    lignes = json.loads(contenu)
    if not isinstance(lignes, list):
        raise ValueError("Liste des occurrences invalide")
    autorisees = set()
    champs = {"source", "ligne", "occurrence", "sha256_ligne", "hmac", "relecteur", "justification"}
    for entree in lignes:
        if not isinstance(entree, dict) or set(entree) not in (champs | {"fichier"}, champs | {"fichier_sha256"}):
            raise ValueError("Entrée incomplète dans la liste des occurrences")
        selecteur = "fichier" if "fichier" in entree else "fichier_sha256"
        nom = entree[selecteur]
        if "fichier_sha256" in entree:
            if not re.fullmatch(r"[a-f0-9]{64}", entree["fichier_sha256"]):
                raise ValueError("Empreinte de chemin invalide")
        if entree["source"] not in {"contenu", "chemin", "message", "titre", "corps"}:
            raise ValueError("Source inconnue")
        if not isinstance(entree["relecteur"], str) or not entree["relecteur"].strip():
            raise ValueError("Relecteur obligatoire")
        if (not isinstance(nom, str) or not nom or nom.startswith("/") or ".." in nom.split("/")
            or any(c in nom for c in "*?[") or not isinstance(entree["justification"], str)
            or not entree["justification"].strip()):
            raise ValueError("Chemin littéral et justification obligatoires")
        for champ in ("ligne", "occurrence"):
            if type(entree[champ]) is not int or entree[champ] < 1:
                raise ValueError("Position précise obligatoire")
        if not all(re.fullmatch(r"[a-f0-9]{64}", entree[c]) for c in ("sha256_ligne", "hmac")):
            raise ValueError("Empreinte complète obligatoire")
        cle = (entree["source"], (selecteur, nom), entree["ligne"], entree["occurrence"], entree["sha256_ligne"], entree["hmac"])
        if cle in autorisees:
            raise ValueError("Occurrence dupliquée")
        autorisees.add(cle)
    return autorisees


class Controle:
    def __init__(self, detecteur, autorisees):
        self.detecteur = detecteur
        self.autorisees = autorisees
        self.refus = 0

    def verifier(self, nom, texte, source):
        nom_hache = hashlib.sha256(os.fsencode(nom)).hexdigest()
        public = nom.isascii() and not self.detecteur.ligne(nom)
        etiquette = ascii(nom) if public else "source-" + empreinte(nom)[:16]
        for numero, ligne in enumerate(texte.splitlines(), 1):
            touches = self.detecteur.ligne(ligne)
            if not touches:
                continue
            identite = hashlib.sha256(ligne.encode("utf-8", errors="surrogatepass")).hexdigest()
            for indice, touche in enumerate(touches, 1):
                marque = touche[-1]
                if any((source, chemin, numero, indice, identite, marque) in self.autorisees
                       for chemin in (("fichier", nom), ("fichier_sha256", nom_hache))):
                    continue
                self.refus += 1
                if self.refus <= 20:
                    print(f"{etiquette}:{numero} — occurrence {indice}, HMAC {marque[:12]}", file=sys.stderr)


def main():
    if not CLE:
        ci = bool(os.environ.get("CI"))
        print(("::error::" if ci else "::warning::") + "Secret CATALOGUE_HMAC_KEY absent.", file=sys.stderr)
        return 2 if ci else 0
    temoin = empreinte("tune-canari")
    if not CANARI:
        # Empreinte publique du message fixe, à épingler par un commit relu.
        print(f"Canari catalogue {temoin}", file=sys.stderr)
        return 2
    if not hmac.compare_digest(temoin, CANARI):
        print("Clé du catalogue inattendue.", file=sys.stderr)
        return 2
    racine = Path(os.fsdecode(subprocess.check_output(["git", "rev-parse", "--show-toplevel"]).rstrip(b"\r\n")))
    def git(*args):
        return subprocess.check_output(["git", "-C", racine, *args], stderr=subprocess.DEVNULL)

    evenement = {}
    if chemin := os.environ.get("GITHUB_EVENT_PATH"):
        evenement = json.loads(Path(chemin).read_text())
    pr = evenement.get("pull_request", {})
    liste = DOSSIER / "occurrences-relues.json"
    if pr:
        # Seule la base déjà relue peut accorder une exception à la contribution.
        base = pr["base"]["sha"]
        if not re.fullmatch(r"[0-9a-f]{40}", base):
            raise ValueError("Révision de confiance invalide")
        chemin_liste = "scripts/catalogue/occurrences-relues.json"
        contenu = git("show", base + ":" + chemin_liste) if git("ls-tree", base, "--", chemin_liste) else b"[]"
    else:
        contenu = liste.read_bytes()
    autorisees = charger_liste(contenu)
    detecteur = Detecteur()
    fichiers = {}
    for ligne_index in git("ls-files", "--stage", "-z").split(b"\0"):
        if not ligne_index:
            continue
        entete, nom = ligne_index.split(b"\t", 1)
        mode, oid, etage = entete.split()
        if etage != b"0":
            raise ValueError("Conflit non résolu dans l’index")
        fichiers[os.fsdecode(nom)] = (mode, oid)

    def lire(nom):
        mode, oid = fichiers[nom]
        if mode == b"160000":
            return oid
        chemin = racine / nom
        return os.fsencode(os.readlink(chemin)) if chemin.is_symlink() else chemin.read_bytes()

    detecteur.preparer(texte_du_fichier(lire(nom)) for nom in fichiers)
    controle = Controle(detecteur, autorisees)
    vus = set()
    for nom in fichiers:
        contenu = lire(nom)
        blob = hashlib.sha1(b"blob " + str(len(contenu)).encode() + b"\0" + contenu).hexdigest().encode()
        vus.add((nom, blob))
        controle.verifier(nom, nom, "chemin")
        controle.verifier(nom, texte_du_fichier(contenu), source="contenu")
    for champ in ("title", "body"):
        if pr.get(champ):
            controle.verifier("PR/" + str(pr.get("number", pr["head"]["sha"])), pr[champ],
                             "titre" if champ == "title" else "corps")
    for sha in revisions(evenement, racine):
        controle.verifier(sha, texte_du_fichier(git("show", "-s", "--format=%B", sha)), "message")
        # Parcourir les deux blobs du diff conserve les numéros de ligne
        # réels, les chemins inhabituels et les changements intermédiaires.
        parents = git("rev-list", "--parents", "-1", sha).split()
        comparaison = [parents[1], sha] if len(parents) > 1 else ["--root", sha]
        changements = git("diff-tree", "-r", "--raw", "--no-abbrev", "--no-commit-id", "--no-renames", "-z", *comparaison).split(b"\0")
        for entete, chemin in zip(changements[::2], changements[1::2]):
            ancien_mode, mode, avant, apres, _ = entete.split()
            nom = os.fsdecode(chemin)
            controle.verifier(nom, nom, "chemin")
            for permissions, blob in ((ancien_mode.lstrip(b":"), avant), (mode, apres)):
                if permissions not in (b"100644", b"100755", b"120000") or (nom, blob) in vus:
                    continue
                vus.add((nom, blob))
                controle.verifier(nom, texte_du_fichier(git("cat-file", "blob", blob)), source="contenu")
    if controle.refus:
        print(f"{controle.refus} occurrence(s) sans autorisation.", file=sys.stderr)
    return int(bool(controle.refus))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError):
        # Les erreurs de lecture peuvent contenir du texte ou un chemin privé.
        print("Garde du catalogue incomplète. Vérifier les sources et la liste relue.", file=sys.stderr)
        sys.exit(2)
