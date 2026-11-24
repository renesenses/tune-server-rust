"""Banc hermétique de la garde, avec des identifiants exclusivement synthétiques."""
import base64
import contextlib
import hashlib
import hmac
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("garde", Path(__file__).with_name("verifier-catalogue.py"))
garde = importlib.util.module_from_spec(spec)
spec.loader.exec_module(garde)
CLE = b"cle-de-banc"
def signature(mot):
    return hmac.digest(CLE, mot.encode(), "sha256").hex()


class GardeCatalogue(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.racine = Path(tmp.name)
        self.addCleanup(os.chdir, Path.cwd())
        os.chdir(self.racine)
        self.git("init", "-q")
        self.liste = self.racine / "scripts/catalogue/occurrences-relues.json"
        self.liste.parent.mkdir(parents=True)
        self.liste.write_text("[]")
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"CI": "true", "GITHUB_EVENT_PATH": "", "GITHUB_SHA": ""}).start()
        patch.multiple(garde, CLE=CLE, CANARI=signature("tune-canari"), DOSSIER=self.liste.parent,
                       PROFILS={signature("exemplealpha"), signature("exemplegamma")},
                       AXES={signature("exemplebeta")}).start()
        garde.empreinte.cache_clear()
        self.addCleanup(garde.empreinte.cache_clear)

    def git(self, *args):
        return subprocess.check_output(["git", "-c", "user.name=Banc", "-c", "user.email=banc@example.invalid", *args], text=True).strip()

    def controle(self):
        with contextlib.redirect_stderr(io.StringIO()) as sortie:
            resultat = garde.main()
        self.assertNotIn("exemplealpha", sortie.getvalue().casefold())
        return resultat

    def test_dix_neuf_formes(self):
        mot = "exemplealpha"
        cas = ["// voir exemplealpha pour la suite", "Le profil exemplealpha est privé",
               "Le profil privé exemplealpha", "profile: EXEMPLEALPHA", "profile: ExempleAlphaV2",
               "let x = profile_exemplealpha;", "profile: exemple-alpha",
               "profile: " + base64.b64encode(mot.encode()).decode(), "profile: " + mot.encode().hex(),
               "profile: exemplеalpha", "profile: ｅxemplealpha", "profile: ExEmPlEaLpHa",
               'x = "exemplealpha"', 'const P = ["exemplealpha"];', 'match p { "exemplealpha" => 1 }',
               "https://exemple.org/exemplealpha", '{"reglages":{"exemplebeta":1}}',
               "profile: exemplealpha".encode("utf-16"), "profiles: exemplealpha; exemplegamma"]
        fichier = self.racine / "note"
        fichier.touch()
        self.git("add", "note")
        for numero, contenu in enumerate(cas, 1):
            with self.subTest(cas=numero):
                fichier.write_bytes(contenu if isinstance(contenu, bytes) else contenu.encode())
                self.assertEqual(self.controle(), 1)

    def entree(self, nom, ligne, indice):
        return {"source": "contenu", "relecteur": "Banc", "fichier": nom, "ligne": 1, "occurrence": indice, "hmac": signature("exemplealpha"),
                "sha256_ligne": hashlib.sha256(ligne.encode()).hexdigest(),
                "justification": "Exemple de documentation relu dans ce banc."}

    def test_occurrence_nominative(self):
        ligne = "usb exemplealpha / exemplealpha"
        self.git("add", "scripts")
        fichier = self.racine / "note"
        fichier.write_text(ligne)
        self.git("add", "note")
        with self.subTest("une occurrence sur deux"):
            self.liste.write_text(json.dumps([self.entree("note", ligne, 1)]))
            self.assertEqual(self.controle(), 1)
        self.liste.write_text(json.dumps([self.entree("note", ligne, i) for i in (1, 2)]))
        self.assertEqual(self.controle(), 0)
        for texte in ("\n"+ligne, ligne+" modifiée", ligne+"\n"+ligne):
            fichier.write_text(texte)
            self.assertEqual(self.controle(), 1)
        fichier.write_text(ligne)
        (self.racine/"autre").write_text(ligne)
        self.git("add", "autre")
        self.assertEqual(self.controle(), 1)

    def test_portee_du_chemin_et_de_la_source(self):
        nom = "exemplealpha.txt"
        (self.racine / nom).write_text(nom)
        self.git("add", nom)
        entree = self.entree(nom, nom, 1)
        del entree["fichier"]
        entree["fichier_sha256"] = hashlib.sha256(nom.encode()).hexdigest()
        self.liste.write_text(json.dumps([entree]))
        self.assertEqual(self.controle(), 1)
        self.liste.write_text(json.dumps([entree, dict(entree, source="chemin")]))
        self.assertEqual(self.controle(), 0)
        self.git("mv", nom, "autre.txt")
        self.assertEqual(self.controle(), 1)

    def test_casse_unicode_et_utf16_grand_boutiste(self):
        with patch.object(garde, "AXES", {signature("exempleiota")}):
            self.assertTrue(list(garde.infractions("EXEMPLEİOTA")))
            texte = garde.texte_du_fichier(b"\xfe\xff" + "exempleiota".encode("utf-16be"))
            self.assertTrue(list(garde.infractions(texte)))

    def test_liste_invalide(self):
        entree = self.entree("note", "ligne", 1)
        for champ, valeur in (("fichier","*"),("ligne",0),("occurrence",True),("hmac",""),
                              ("justification"," "),("relecteur",""),("source","*")):
            with self.subTest(champ=champ), self.assertRaises(ValueError):
                garde.charger_liste(json.dumps([dict(entree, **{champ:valeur})]))

    def test_secret_et_canari(self):
        with patch.object(garde, "CLE", b""):
            self.assertEqual(self.controle(), 2)
            with patch.dict(os.environ, {"CI": ""}):
                self.assertEqual(self.controle(), 0)
        with patch.object(garde, "CANARI", ""):
            self.assertEqual(self.controle(), 2)
        with patch.object(garde, "CANARI", "0"*64):
            self.assertEqual(self.controle(), 2)

    def test_revisions_et_autorisation_de_la_base(self):
        fichier = self.racine / "note"
        self.git("add", "scripts")
        self.git("commit", "-qm", "base")
        base = self.git("rev-parse", "HEAD")
        fichier.write_text("exemplealpha")
        self.liste.write_text(json.dumps([self.entree("note", "exemplealpha", 1)]))
        self.git("add", ".")
        self.git("commit", "-qm", "intermediaire")
        self.git("rm", "-q", "note")
        self.git("commit", "-qm", "retrait")
        evenement = self.racine/"evenement.json"
        pr = {"base":{"sha":base}, "head":{"sha":self.git("rev-parse","HEAD")}, "title":"public", "body":""}
        evenement.write_text(json.dumps({"pull_request":pr}))
        with patch.dict(os.environ, {"GITHUB_EVENT_PATH":str(evenement)}):
            self.assertEqual(self.controle(), 1)
            self.git("checkout", "-q", base)
            pr["head"]["sha"] = base
            evenement.write_text(json.dumps({"pull_request":pr}))
            self.assertEqual(self.controle(), 0)
            for champ in ("title","body"):
                evenement.write_text(json.dumps({"pull_request":dict(pr, **{champ:"exemplealpha"})}))
                self.assertEqual(self.controle(), 1)
            self.git("commit", "-q", "--allow-empty", "-m", "exemplealpha")
            pr["head"]["sha"] = self.git("rev-parse", "HEAD")
            evenement.write_text(json.dumps({"pull_request":pr}))
            self.assertEqual(self.controle(), 1)


if __name__ == "__main__":
    unittest.main()
