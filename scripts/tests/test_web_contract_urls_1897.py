#!/usr/bin/env python3
"""Régressions d'extraction des URL réelles du client, sans évaluer JavaScript."""
import contextlib
import io
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "web-contract-map.py"
spec = importlib.util.spec_from_file_location("web_contract_map_1897", SCRIPT)
cartographe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cartographe)

TYPES = {"types.ts": "export interface Track {\n id: number;\n title: string;\n}"}


class UrlsContrat(unittest.TestCase):
    def extraire(self, appel):
        with contextlib.redirect_stderr(io.StringIO()):
            return cartographe.carte(TYPES, appel)

    def exige(self, appel, route, methode="GET", champs=("id", "title")):
        entries, non_resolus = self.extraire(appel)
        self.assertFalse(non_resolus, f"URL valide perdue: {non_resolus}")
        self.assertEqual(len(entries), 1, entries)
        self.assertEqual(entries[0]["route"], route, "mauvais chemin extrait")
        self.assertEqual(entries[0]["methode"], methode)
        self.assertEqual(entries[0]["champs_obligatoires"], list(champs))
        return entries[0]

    def test_url_pistes_album_du_client_epingle(self):
        entry = self.exige(
            "fetchJSON<Track[]>(`${BASE}/library/albums/${id}/tracks${qs ? `?${qs}` : ''}`);",
            "/library/albums/{}/tracks",
        )
        self.assertTrue(entry["liste"], "les pistes d'album doivent rester un contrat de liste")

    def test_type_porte_par_le_retour_et_methode_apres_le_gabarit(self):
        self.exige(
            """export function pistes(): Promise<{ items: string[] }> {
                return fetchJSON(`${BASE}/library/albums/${id}/tracks${qs ? `?${qs}` : ''}`,
                    { method: 'POST' });
            }""",
            "/library/albums/{}/tracks", "POST", ("items",),
        )

    def test_gabarit_dans_un_parametre_de_chemin(self):
        self.exige(
            "fetchJSON<Track>(`${BASE}/devices/${encodeURIComponent(`prefix-${id}`)}/track`);",
            "/devices/{}/track",
        )

    def test_gabarit_dans_une_requete_deja_ouverte(self):
        self.exige(
            "fetchJSON<Track[]>(`${BASE}/tracks?filter=${flag ? `nom-${id}` : ''}`);",
            "/tracks",
        )

    def test_accolades_et_point_interrogation_dans_une_chaine(self):
        self.exige(
            """fetchJSON<Track>(`${BASE}/tracks/${id.replace("}", "{?")}/metadata`);""",
            "/tracks/{}/metadata",
        )

    def test_commentaires_et_delimiteur_echappe(self):
        self.exige(
            r"""fetchJSON<Track>(`${BASE}/tracks/${id /* } ` */ + "\""}/metadata`);""",
            "/tracks/{}/metadata",
        )

    def test_url_tronquee_reste_signalee(self):
        for appel in [
            "fetchJSON<Track[]>(`${BASE}/tracks${qs ? `?${qs}` : ''}",
            "fetchJSON<Track[]>(`${BASE}/tracks/${id",
        ]:
            with self.subTest(appel=appel):
                entries, non_resolus = self.extraire(appel)
                self.assertFalse(entries, "une URL tronquee ne doit pas fabriquer un contrat")
                self.assertEqual(len(non_resolus), 1)
                self.assertIn("route non fiable", non_resolus[0]["raison"])

    def test_regexp_non_interpretee_reste_signalee(self):
        entries, non_resolus = self.extraire(
            r"fetchJSON<Track>(`${BASE}/tracks/${id.replace(/}/g, '')}`);"
        )
        self.assertFalse(entries, "une regexp non comprise ne doit pas fabriquer un chemin")
        self.assertEqual(len(non_resolus), 1)

    def test_une_branche_de_chemin_ne_devient_pas_une_requete(self):
        entries, non_resolus = self.extraire(
            "fetchJSON<Track[]>(`${BASE}/tracks${qs ? `?${qs}` : '/other'}`);"
        )
        self.assertFalse(entries, "un suffixe qui peut changer le chemin reste non resolu")
        self.assertEqual(len(non_resolus), 1)

    def test_un_parametre_conditionnel_entier_reste_un_parametre(self):
        self.exige(
            "fetchJSON<Track>(`${BASE}/tracks/${id ? 'one' : 'two'}/metadata`);",
            "/tracks/{}/metadata",
        )

    def test_la_methode_du_voisin_ne_contamine_pas_la_route(self):
        entries, nr = self.extraire(
            "fetchJSON<Track[]>(`${BASE}/tracks${qs ? `?${qs}` : ''}`);\n"
            "fetchJSON<Track>(`${BASE}/tracks/${id}`, {method: 'PUT'});"
        )
        self.assertFalse(nr)
        self.assertEqual({(e["route"], e["methode"]) for e in entries},
                         {("/tracks", "GET"), ("/tracks/{}", "PUT")})

    def test_suffixe_requete_concatene(self):
        self.exige(
            "fetchJSON<Track[]>(`${BASE}/radios${qs ? '?' + qs : ''}`);",
            "/radios",
        )

    def test_suffixe_variable_garde_la_notation_historique(self):
        self.exige(
            "fetchJSON<Track[]>(`${BASE}/radios${qs}`);",
            "/radios{}",
        )

    def test_limite_imbrication_sans_recursion_non_bornee(self):
        expr = "id"
        for _ in range(80):
            expr = "`${" + expr + "}`"
        entries, nr = self.extraire("fetchJSON<Track>(`${BASE}/tracks/${" + expr + "}`);")
        self.assertFalse(entries)
        self.assertEqual(len(nr), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
