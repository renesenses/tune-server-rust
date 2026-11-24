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
        with patch.dict(os.environ, {"CI": ""}), patch.object(garde, "CLE", b""), contextlib.redirect_stderr(io.StringIO()) as erreurs:
            self.assertEqual(garde.main(), 0)
            self.assertIn("::warning::", erreurs.getvalue())

    def test_secret_absent_en_ci(self):
        with patch.dict(os.environ, {"CI": "true"}), patch.object(garde, "CLE", b""), contextlib.redirect_stderr(io.StringIO()) as erreurs:
            self.assertNotEqual(garde.main(), 0)
            self.assertIn("CATALOGUE_HMAC_KEY", erreurs.getvalue())

    def test_historique_et_metadonnees(self):
        with tempfile.TemporaryDirectory() as tmp:
            racine = Path(tmp)
            def git(*args):
                return subprocess.check_output(["git", "-C", tmp, "-c", "user.name=Test",
                    "-c", "user.email=test@example.invalid", *args], text=True).strip()
            git("init", "-q")
            git("commit", "-q", "--allow-empty", "-m", "base")
            base = git("rev-parse", "HEAD")
            (racine / "note").write_text("ajout intermediaire")
            git("add", "note")
            git("commit", "-q", "-m", "message intermediaire")
            git("rm", "-q", "note")
            git("commit", "-q", "-m", "retrait")
            pr = {"base": {"sha": base}, "head": {"sha": git("rev-parse", "HEAD")},
                  "title": "titre temoin", "body": "corps temoin"}
            textes = dict(garde.textes_de_revision(racine, {"pull_request": pr}))
            self.assertEqual(textes["PR/title"], pr["title"])
            self.assertEqual(textes["PR/body"], pr["body"])
            self.assertTrue(any("message intermediaire" in t for t in textes.values()))
            self.assertTrue(any("+ajout intermediaire" in t for t in textes.values()))
        self.assertEqual(garde.texte_du_fichier("repère".encode("utf-16")), "repère")

    @patch.dict(os.environ, {"GITHUB_EVENT_PATH": ""})
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
                        self.assertNotIn("exemple", erreurs.getvalue().casefold())
                        self.assertRegex(erreurs.getvalue(), r"HMAC [0-9a-f]{12}")
            finally:
                os.chdir(precedent)


if __name__ == "__main__":
    unittest.main()
