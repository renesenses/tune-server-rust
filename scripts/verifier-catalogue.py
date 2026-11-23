"""Contrôle du vocabulaire du catalogue dans les fichiers suivis par Git."""
from functools import lru_cache
import hmac
import os
from pathlib import Path
import re
import subprocess
import sys

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
LISTE = re.compile(
    r"""(?<=[/(])(?:[A-Za-z]+\|)+[A-Za-z]+(?=[/)])"""
    r"""|["'`][A-Za-z]+["'`](?:[ \t]*,[ \t]*["'`][A-Za-z]+["'`])+"""
)


@lru_cache(maxsize=8192)
def empreinte(mot):
    return hmac.digest(CLE, mot.lower().encode("utf-8"), "sha256").hex()


def correspond(mot, empreintes):
    return empreinte(mot) in empreintes


def infractions(texte):
    for liste in LISTE.finditer(texte):
        mots = set(JETON.findall(liste.group().lower()))
        for groupe in (PROFILS, AXES):
            if sum(correspond(mot, groupe) for mot in mots) >= 2:
                for mot in sorted(mots):
                    if correspond(mot, groupe):
                        yield texte.count("\n", 0, liste.start()) + 1, mot
                break

    exemple = EXEMPLE.fullmatch(texte)
    if exemple:
        groupe = exemple.lastgroup
        if correspond(exemple[groupe], PROFILS if groupe == "profil" else AXES):
            yield texte.count("\n", 0, exemple.start(groupe)) + 1, exemple[groupe]

    for structure in REGLAGES.finditer(texte):
        fin = fin_bloc(texte, structure.end() - 1)
        for champ in CHAMP.finditer(texte, structure.end(), fin):
            if correspond(champ["cle"], AXES):
                yield texte.count("\n", 0, champ.start("cle")) + 1, champ["cle"]

    for contexte in CONTEXTE.finditer(texte):
        nom = contexte["nom"].lower()
        empreintes = PROFILS if nom.startswith("prof") else AXES
        if nom.startswith("catalog"):
            if not any(c in contexte.group() for c in ":=|."):
                continue
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
                yield texte.count("\n", 0, jeton.start()) + 1, jeton.group()


def main():
    if not CLE:
        print("::warning::Garde du catalogue désactivée, secret CATALOGUE_HMAC_KEY absent.", file=sys.stderr)
        return 0
    racine = Path(os.fsdecode(subprocess.check_output(
        ["git", "rev-parse", "--show-toplevel"]
    ).rstrip(b"\r\n")))
    fichiers = subprocess.check_output(["git", "-C", racine, "ls-files", "-z"])
    problemes = False
    for nom in fichiers.split(b"\0")[:-1]:
        chemin = racine / os.fsdecode(nom)
        contenu = os.fsencode(os.readlink(chemin)) if chemin.is_symlink() else chemin.read_bytes()
        for ligne, mot in sorted(set(infractions(contenu.decode("utf-8", errors="replace")))):
            print(f"{os.fsdecode(nom)}:{ligne} — référence au catalogue privé ({mot})", file=sys.stderr)
            problemes = True
    return int(problemes)


if __name__ == "__main__":
    sys.exit(main())
