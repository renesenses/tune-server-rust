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


# Les commentaires d'un type ne déclarent aucun champ (carte a10e41183, #9362).
#
# `StreamingPlaylist` documente `editable` par « `false` : il la suit seulement
# (éditoriale, autre compte) ; absent : le serveur ne le dit pas ». Le `;` de
# la PROSE coupait le bloc, et « absent : le serveur… » était lu comme un champ
# obligatoire. `GET /system/env` héritait de même d'`absent` par la doc de
# `data_persistence`. Le serveur n'a jamais rendu ce champ : le banc
# `web_response_contracts` rougissait sur une exigence que le client n'a pas.
TYPES_DOCUMENTES = {"types.ts": """
export interface StreamingPlaylist {
  source_id: string;
  /**
   * `false` : il la suit seulement (éditoriale, autre compte) ; absent : le
   * serveur ne le dit pas.
   */
  editable?: boolean | null;
  // repli ; absent : serveur plus ancien
  source: string;
}
export interface AvecVraiChamp {
  /** Témoin ; absent : ce mot n'est ici qu'un commentaire. */
  absent: boolean;
  source_id: string;
}
"""}


class CommentairesDesTypes(unittest.TestCase):
    def extraire(self, appel, types=TYPES_DOCUMENTES):
        with contextlib.redirect_stderr(io.StringIO()):
            return cartographe.carte(types, appel)

    def contrat(self, appel, types=TYPES_DOCUMENTES):
        entries, non_resolus = self.extraire(appel, types)
        self.assertFalse(non_resolus, non_resolus)
        self.assertEqual(len(entries), 1, entries)
        return entries[0]

    def test_temoin_la_prose_d_un_commentaire_n_est_pas_un_champ(self):
        e = self.contrat("fetchJSON<StreamingPlaylist[]>(`${BASE}/streaming/${s}/mixes`);")
        self.assertEqual(e["champs_obligatoires"], ["source_id", "source"],
                         "un mot de commentaire est exigé du serveur")
        self.assertEqual(e["champs_optionnels"], ["editable"])

    def test_temoin_type_en_ligne_documente(self):
        e = self.contrat("""fetchJSON<{
            TUNE_PORT?: string;
            /**
             * `null` hors conteneur ; absent : serveur plus ancien.
             */
            data_persistence?: string | null;
        }>(`${BASE}/system/env`);""")
        self.assertEqual(e["champs_obligatoires"], [],
                         "GET /system/env n'exige aucun champ du serveur")

    @staticmethod
    def manquants(entree, reponse):
        """Ce que le banc reprocherait à la réponse : les obligatoires absents."""
        return [c for c in entree["champs_obligatoires"] if c not in reponse]

    def test_temoin_une_reponse_sans_le_mot_du_commentaire_est_conforme(self):
        e = self.contrat("fetchJSON<StreamingPlaylist[]>(`${BASE}/streaming/${s}/mixes`);")
        self.assertEqual(self.manquants(e, {"source_id": "1", "source": "qobuz"}), [])

    def test_contre_temoin_un_vrai_champ_obligatoire_reste_exige(self):
        # Même nom, mais DÉCLARÉ : le retirer du serveur doit toujours rougir.
        e = self.contrat("fetchJSON<AvecVraiChamp>(`${BASE}/avec/vrai-champ`);")
        self.assertEqual(e["champs_obligatoires"], ["absent", "source_id"])
        self.assertEqual(self.manquants(e, {"source_id": "1"}), ["absent"],
                         "une réponse privée d'un vrai champ obligatoire doit échouer")

    def test_contre_temoin_une_chaine_avec_double_barre_n_est_pas_un_commentaire(self):
        e = self.contrat(
            "fetchJSON<{ lien: 'http://x' | 'https://y'; requis: string }>(`${BASE}/liens`);"
        )
        self.assertEqual(e["champs_obligatoires"], ["lien", "requis"])

    # Relecture de #9435 : une accolade dans une JSDoc ne ferme pas le type.
    def test_temoin_accolade_dans_un_commentaire_ne_tronque_pas_le_type(self):
        types = {"types.ts": """
export interface Enveloppe {
  /** Forme `{ items }` ; l'ancienne était `}` seule. */
  items: string[];
  total: number;
}
"""}
        e = self.contrat("fetchJSON<Enveloppe>(`${BASE}/enveloppe`);", types)
        self.assertEqual(e["champs_obligatoires"], ["items", "total"])


# Suffixe de requête porté par une constante locale (carte a10e41183, #9362).
#
# `getArtistTracks` est passée de `…/tracks` à `…/tracks${suite}`, avec
# `const suite = dossier ? `?${…}` : '';`. La carte nommait alors la route
# `/library/artists/{}/tracks{}`, que le serveur ne sert pas, et perdait le
# contrat de `/library/artists/{}/tracks` : `library_1897` paniquait.
class SuffixeDeRequeteLocal(UrlsContrat):
    def test_temoin_constante_locale_de_requete(self):
        self.exige(
            """export function getArtistTracks(id: number, dossier?: string | null) {
              const suite = dossier ? `?${new URLSearchParams({ folder: dossier })}` : '';
              return fetchJSON<Track[]>(`${BASE}/library/artists/${id}/tracks${suite}`);
            }""",
            "/library/artists/{}/tracks",
        )

    def test_temoin_requete_locale_dans_un_retour_de_fonction(self):
        self.exige(
            """export function onde(id: number, points?: number): Promise<{ id: number; title: string }> {
              const q = points ? `?points=${Math.round(points)}` : '';
              return fetchJSON(`${BASE}/library/tracks/${id}/waveform${q}`);
            }""",
            "/library/tracks/{}/waveform",
        )

    def test_contre_temoin_constante_qui_change_le_chemin(self):
        self.exige(
            """export function f(id: number, x?: boolean) {
              const suite = x ? '/extra' : '';
              return fetchJSON<Track[]>(`${BASE}/radios${suite}`);
            }""",
            "/radios{}",
        )

    def test_contre_temoin_constante_d_une_autre_fonction(self):
        self.exige(
            """export function a(d?: string) {
              const suite = d ? `?d=${d}` : '';
              return suite;
            }
            export function b() {
              return fetchJSON<Track[]>(`${BASE}/radios${suite}`);
            }""",
            "/radios{}",
        )

    # Relecture de #9435 : une RÉAFFECTATION après la déclaration peut faire
    # du suffixe un segment de chemin ; le retirer masquerait la vraie route.
    def test_contre_temoin_reaffectation_composee(self):
        self.exige(
            """export function f(x?: string) {
              let q = x ? `?x=${x}` : '';
              q += '/extra';
              return fetchJSON<Track[]>(`${BASE}/radios${q}`);
            }""",
            "/radios{}",
        )

    def test_contre_temoin_reaffectation_simple(self):
        self.exige(
            """export function f(x?: string) {
              let q = x ? `?x=${x}` : '';
              if (x === 'a') q = '/x';
              return fetchJSON<Track[]>(`${BASE}/radios${q}`);
            }""",
            "/radios{}",
        )

    def test_temoin_comparaison_n_est_pas_une_reaffectation(self):
        self.exige(
            """export function f(x?: string) {
              const q = x ? `?x=${x}` : '';
              if (q === '' || q == null) console.log(q);
              return fetchJSON<Track[]>(`${BASE}/radios${q}`);
            }""",
            "/radios",
        )

    # Une méthode d'objet est une fonction : sa constante n'appartient pas à
    # la méthode voisine.
    def test_contre_temoin_constante_d_une_methode_voisine(self):
        self.exige(
            """export const api = {
              a(d?: string) {
                const suite = d ? `?d=${d}` : '';
                return suite;
              },
              b() {
                return fetchJSON<Track[]>(`${BASE}/radios${suite}`);
              },
            };""",
            "/radios{}",
        )

    def test_temoin_un_bloc_if_ne_borne_pas_la_fonction(self):
        self.exige(
            """export function f(d?: string) {
              const suite = d ? `?d=${d}` : '';
              if (d) {
                return fetchJSON<Track[]>(`${BASE}/radios${suite}`);
              }
            }""",
            "/radios",
        )

    # `/${suite}` est un segment de chemin, même si la constante ressemble à
    # une requête : la garde qui le protège doit rester (mutant tué).
    def test_contre_temoin_suffixe_apres_une_barre_reste_un_parametre(self):
        self.exige(
            """export function f(d?: string) {
              const suite = d ? `?d=${d}` : '';
              return fetchJSON<Track[]>(`${BASE}/radios/${suite}`);
            }""",
            "/radios/{}",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
