"""Exerce la garde avec deux identifiants fictifs, dans un dépôt indépendant."""
import contextlib
import hashlib
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
    def test_fichiers_suivis_depuis_un_sous_dossier(self):
        profil = hashlib.sha256(b"exemplealpha").hexdigest()
        second = hashlib.sha256(b"exemplegamma").hexdigest()
        axe = hashlib.sha256(b"exemplebeta").hexdigest()
        with tempfile.TemporaryDirectory() as dossier, patch.object(garde, "PROFILS", {profil, second}), patch.object(garde, "AXES", {axe}):
            racine = Path(dossier)
            subprocess.run(["git", "init", "-q", dossier], check=True)
            docs = racine / "notes"
            docs.mkdir()
            source = docs / "hors SDK.sans-extension"
            source.write_text("profil public\n<x:exemplebeta/>", encoding="utf-8")
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
                        self.assertNotIn("exemplealpha", erreurs.getvalue())
            finally:
                os.chdir(precedent)


if __name__ == "__main__":
    unittest.main()
