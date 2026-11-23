"""Exerce la garde avec deux identifiants fictifs, dans un dépôt indépendant."""
import contextlib
import hmac
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("garde", Path(__file__).with_name("verifier-catalogue.py"))
garde = importlib.util.module_from_spec(spec)
spec.loader.exec_module(garde)


class GardeCatalogue(unittest.TestCase):
    def test_secret_absent(self):
        with patch.object(garde, "CLE", b""), contextlib.redirect_stderr(io.StringIO()) as erreurs:
            self.assertEqual(garde.main(), 0)
            self.assertIn("::warning::", erreurs.getvalue())

    def test_fichiers_suivis_depuis_un_sous_dossier(self):
        cle = b"cle-de-test"
        profil = hmac.digest(cle,b"exemplealpha","sha256").hex()
        second = hmac.digest(cle,b"exemplegamma","sha256").hex()
        axe = hmac.digest(cle,b"exemplebeta","sha256").hex()
        with tempfile.TemporaryDirectory() as dossier, patch.object(garde, "CLE", cle), patch.object(garde, "PROFILS", {profil, second}), patch.object(garde, "AXES", {axe}):
            racine = Path(dossier)
            subprocess.run(["git", "init", "-q", dossier], check=True)
            docs = racine / "notes"
            docs.mkdir()
            source = docs / "hors SDK.sans-extension"
            source.write_text("profil public\n<x:exemplebeta/>\nExempleAlpha | ExempleGamma\nusb exemplealpha, exemplegamma 2\ncatalog exemplebeta", encoding="utf-8")
            subprocess.run(["git", "-C", dossier, "add", "."], check=True)
            precedent = Path.cwd()
            try:
                os.chdir(docs)
                self.assertEqual(garde.main(), 0)
                for contenu in (
                    "Profils du catalogue :\n- autre\n- ExempleAlpha\n",
                    'const PROFILES: &[&str] = &["exemplealpha"];',
                    '{"declarations": {\n "exemplebeta": "valeur"\n}}',
                    'pub struct UpsamplerSettings { pub exemplebeta: String }',
                    '"exemplealpha"',
                    "const MOTIFS = /exemplealpha|exemplegamma/;",
                    '{"exemplebeta":"valeur"}',
                ):
                    with self.subTest(contenu=contenu), contextlib.redirect_stderr(io.StringIO()) as erreurs:
                        source.write_text(contenu, encoding="utf-8")
                        self.assertEqual(garde.main(), 1)
                        self.assertIn(source.name, erreurs.getvalue())
                        self.assertRegex(erreurs.getvalue().lower(), r"\(exemple(?:alpha|gamma|beta)\)")
            finally:
                os.chdir(precedent)


if __name__ == "__main__":
    unittest.main()
