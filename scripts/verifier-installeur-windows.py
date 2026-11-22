#!/usr/bin/env python3
"""Compile le NSIS de release.yml, sans installer ni lancer de programme.

Les fichiers embarqués sont factices : ce banc vérifie le script et ses deux
variantes (avec/sans outils facultatifs), pas le fonctionnement de Windows.
"""

import argparse
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap


def script_nsis(workflow: str, outils: bool) -> str:
    debut = workflow.index("- name: Create installer (windows)")
    bloc = re.search(
        r'^          @"\n(.*?)^          "@ \| Out-File -Encoding ASCII installer\.nsi',
        workflow[debut:], re.MULTILINE | re.DOTALL,
    )
    if bloc is None:
        raise ValueError("Le script NSIS de livraison est introuvable")
    source = textwrap.dedent(bloc.group(1))
    # Reproduire les substitutions de ce here-string PowerShell. Préserver
    # d'abord ses dollars échappés : ce sont les variables/macros de NSIS.
    dollar = "__DOLLAR_NSIS_8861__"
    source = source.replace("`$", dollar)
    source = source.replace("${{ matrix.target }}", "x86_64-pc-windows-msvc")
    source = source.replace("${{ env.ARTIFACT }}", "tune-verification")
    source = source.replace("${version}", "0.0.0-verification")
    repertoire = r"target\x86_64-pc-windows-msvc\release"
    for variable, fichiers in (
        ("daemonFile", ["airplay-daemon.exe"]),
        ("ffmpegFile", ["ffmpeg.exe", "FFMPEG-LICENSE.txt"]),
        ("fpcalcFile", ["fpcalc.exe"]),
    ):
        directives = "\n".join(f'File "{repertoire}\\{f}"' for f in fichiers)
        source = source.replace(f"${variable}", directives if outils else "")
    # Une variable PowerShell ajoutée ou un dollar NSIS non échappé ne doit
    # pas être silencieusement supprimé par le banc.
    reste = re.search(r"\$[A-Za-z_{]", source)
    if reste or "`" in source:
        raise ValueError("Interpolation PowerShell non couverte dans le NSIS")
    return source.replace(dollar, "$")


def compiler(makensis: str, workflow: str, outils: bool) -> None:
    source = script_nsis(workflow, outils)
    with tempfile.TemporaryDirectory(prefix="tune-nsis-") as temporaire:
        dossier = Path(temporaire)
        # Les chemins viennent du script lui-même : un nouveau fichier livré
        # est inclus dans le banc sans maintenir une seconde liste.
        for chemin in re.findall(r'^\s*File(?: /r)? "([^"]+)"\s*$', source, re.MULTILINE):
            chemin = chemin.replace("\\", "/").replace("*.*", "temoin.txt")
            destination = dossier / chemin
            if not destination.resolve().is_relative_to(dossier.resolve()):
                raise ValueError(f"Fichier NSIS hors du banc : {chemin}")
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(b"Fichier factice du banc NSIS, ne pas executer.\n")
        nsis = dossier / "installer.nsi"
        # Out-File -Encoding ASCII dans le workflow de livraison.
        nsis.write_bytes(source.encode("ascii", errors="replace"))
        subprocess.run([makensis, str(nsis)], cwd=dossier, check=True)
        resultat = dossier / "tune-verification-setup.exe"
        if not resultat.is_file() or resultat.stat().st_size == 0:
            raise RuntimeError("makensis n'a pas produit d'installeur")
        print(f"NSIS compilé (outils facultatifs={outils}), sans exécution", flush=True)


def main() -> None:
    arguments = argparse.ArgumentParser(description=__doc__)
    arguments.add_argument("--makensis", default="makensis")
    arguments.add_argument(
        "--workflow", type=Path,
        default=Path(__file__).resolve().parents[1] / ".github/workflows/release.yml",
    )
    options = arguments.parse_args()
    workflow = options.workflow.read_text(encoding="utf-8")
    for outils in (False, True):
        compiler(options.makensis, workflow, outils)


if __name__ == "__main__":
    main()
