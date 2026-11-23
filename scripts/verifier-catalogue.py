"""Contrôle du vocabulaire du catalogue dans les fichiers suivis par Git."""
from functools import lru_cache
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys

PROFILS = {
    "12765295db4f1e7911ddb4f051191091a0432570270e1fca2076e718e61ddae0",
    "6f61f46c23a66607af759dcd348ac132846385d8f39534e69b6cd934bdabad8b",
    "38a3c50b7c1f9635ef1550743eca97c388dc207829c5c204db4f3c1511db77f9",
}
AXES = {
    "04a9fba2f0c404565e6b9c8a4eca48d424f229222e391ab82ccfdb98958925db",
    "d34a569ab7aaa54dacd715ae64953455d86b768846cd0085ef4e9e7471489b7b",
    "263a4dbe41488fb87214b0032339dbb9f0c8da14c16dfcf13084bf3c2552eca5",
    "c796d37c6d491e8f0c6e9b83eed34c15c0f377f9f0f3cbb3216fbbf776da6325",
}

# Une déclaration peut être du JSON, un champ Rust, un exemple de
# configuration ou une énumération dans la documentation.
CONTEXTE = re.compile(
    r"(?<![^\W_])(?P<nom>profils?|profiles?|catalog(?:ue)?s?|axes?|"
    r"d[ée]clarations?)(?![^\W_])"
    r"(?:[_ -](?:id|name|nom|disponibles?|pris en charge|du catalogue|"
    r"de cadence|de sur[ée]chantillonnage|par d[ée]faut|defaults?))*"
    r"""[\x20\t"'`\]]*(?:(?:=>|[:=|])[ \t]*|\.(?=[A-Za-z_])|[ \t]+)""",
    re.IGNORECASE,
)
# Dans une constante typée, les valeurs suivent le type et l'affectation.
TYPE_TABLEAU = re.compile(r"(?:&?\[[^\]\n]*\]|(?:Vec|Option)<[^>\n]+>)\s*=\s*&?")
JETON = re.compile(r"[A-Za-z_][A-Za-z_0-9]*")
MOT = r"""(?:«[ \t]*)?["'`*]*[A-Za-z_][A-Za-z_0-9]*["'`»*]*"""
VALEURS = re.compile(rf"{MOT}(?:(?:[ \t]*[,|/][ \t]*|[ \t]+(?:ou|et|or|and)[ \t]+){MOT})*")
PAIRES = {"[": "]", "{": "}", "(": ")"}
STRUCTURE = re.compile(r"""[\[\]{}()"\\]""")


def fin_bloc(texte, debut):
    """Respecte les chaînes JSON et les délimiteurs imbriqués."""
    fins = [PAIRES[texte[debut]]]
    chaine = False
    ignorer = -1
    for marque in STRUCTURE.finditer(texte, debut + 1):
        pos = marque.start()
        if pos == ignorer:
            continue
        signe = marque.group()
        if signe == "\\" and chaine:
            ignorer = pos + 1
        elif signe == '"':
            chaine = not chaine
        elif not chaine:
            if signe in PAIRES:
                fins.append(PAIRES[signe])
            elif signe == fins[-1]:
                fins.pop()
                if not fins:
                    return pos
    return texte.find("\n", debut) if "\n" in texte[debut:] else len(texte)


# Exemples autonomes et déclarations de réglages dans le code ou les schémas.
EXEMPLE = re.compile(
    r"""\s*(?:["'`](?P<profil>\w+)["'`]|"""
    r"""\{\s*["'](?P<axe>\w+)["']\s*:\s*["'][^"'\n]*["']\s*\})\s*"""
)
REGLAGES = re.compile(r"\b(?:UpsamplerSettings|ReglagesSurechantillonnage)\s*\{")
CHAMP = re.compile(r"""(?<!\w)["']?(?P<cle>[A-Za-z_][A-Za-z_0-9]*)["']?\s*:(?!:)""")
PUCES = re.compile(r"(?:[-*][ \t]+" + MOT + r"[ \t]*(?:\r?\n[ \t]*|$))+")


# Une garde ou un export peut énumérer les identifiants sans nommer le
# catalogue. Reconnaître aussi ces listes littérales et alternatives de regex.
LISTE = re.compile(r"""\b[A-Za-z]+(?:["'`]?[ \t]*[,|][ \t]*["'`]?[A-Za-z]+\b){1,}""")


@lru_cache(maxsize=8192)
def empreinte(mot):
    return hashlib.sha256(mot.lower().encode("utf-8")).hexdigest()


def correspond(mot, empreintes):
    return empreinte(mot) in empreintes


def infractions(texte):
    for liste in LISTE.finditer(texte):
        mots = set(JETON.findall(liste.group().lower()))
        for groupe in (PROFILS, AXES):
            if sum(correspond(mot, groupe) for mot in mots) >= 2:
                yield texte.count("\n", 0, liste.start()) + 1
                break

    exemple = EXEMPLE.fullmatch(texte)
    if exemple:
        groupe = exemple.lastgroup
        if correspond(exemple[groupe], PROFILS if groupe == "profil" else AXES):
            yield texte.count("\n", 0, exemple.start(groupe)) + 1

    for structure in REGLAGES.finditer(texte):
        fin = fin_bloc(texte, structure.end() - 1)
        for champ in CHAMP.finditer(texte, structure.end(), fin):
            if correspond(champ["cle"], AXES):
                yield texte.count("\n", 0, champ.start("cle")) + 1

    for contexte in CONTEXTE.finditer(texte):
        nom = contexte["nom"].lower()
        empreintes = PROFILS if nom.startswith("prof") else AXES
        if nom.startswith("catalog"):
            empreintes = PROFILS | AXES
        debut = contexte.end()
        while debut < len(texte) and texte[debut].isspace():
            debut += 1
        if (type_tableau := TYPE_TABLEAU.match(texte, debut)):
            debut = type_tableau.end()
        if texte[debut:debut + 1] in PAIRES:
            fin = fin_bloc(texte, debut)
        elif (puces := PUCES.match(texte, debut)):
            fin = puces.end()
        else:
            # Les énumérations libres sont limitées à la ligne du champ.
            fin_ligne = texte.find("\n", debut)
            valeur = VALEURS.match(texte, debut, len(texte) if fin_ligne < 0 else fin_ligne)
            if not valeur:
                continue
            fin = valeur.end()
        for jeton in JETON.finditer(texte, debut, fin):
            if correspond(jeton.group(), empreintes):
                yield texte.count("\n", 0, jeton.start()) + 1


def main():
    racine = Path(os.fsdecode(subprocess.check_output(
        ["git", "rev-parse", "--show-toplevel"]
    ).rstrip(b"\r\n")))
    fichiers = subprocess.check_output(["git", "-C", racine, "ls-files", "-z"])
    problemes = False
    for nom in fichiers.split(b"\0")[:-1]:
        chemin = racine / os.fsdecode(nom)
        contenu = os.fsencode(os.readlink(chemin)) if chemin.is_symlink() else chemin.read_bytes()
        for ligne in sorted(set(infractions(contenu.decode("utf-8", errors="replace")))):
            print(f"{os.fsdecode(nom)}:{ligne} — référence au catalogue privé", file=sys.stderr)
            problemes = True
    return int(problemes)


if __name__ == "__main__":
    sys.exit(main())
