# #4805, étape A : `identify-all` mesuré sur une copie de bibliothèque

Mesure du 13/10/2026, sur Shrek, avec le banc
`tune-server/src/routes/library/identification_lot/mesure_a.rs` (essai
`#[ignore]`, lancé à la main).

## Ce qui a été mesuré, et sur quoi

- **La boucle de la route elle-même**, `executer_le_lot`, avec la même sélection
  (`sql_candidats_identification`), la même cadence MusicBrainz, le même
  disjoncteur, les mêmes écritures. Seules la garde Premium et la réponse HTTP
  sont sautées. Les crédits après identification ne tournent pas : ils
  demandent le droit `AutoEnrichment`, absent du banc.
- **Une COPIE** de la base de bibliothèque du 23/09/2026 : celle dont #4805 §1
  tire ses chiffres (3 938 albums locaux, 29 identifiés, 388 pistes avec un
  `musicbrainz_recording_id`). La copie est prise dans un dossier à part, avec
  un `HOME` dédié. Aucune base en service n'a été ouverte.
- Code : `chantier/enrichissement-metarust` @ `a7c12e959`. Il contient les
  étapes 1, 1 bis, 2, B et D de #4805 et le choix d'édition sûr de MetaRust.

## Résultat

| | avant | après | écart |
|---|---:|---:|---:|
| albums locaux avec `musicbrainz_release_id` | 29 | **2 128** | +2 099 |
| albums locaux avec `musicbrainz_release_group_id` | 0 | **2 099** | +2 099 |
| pistes locales avec `musicbrainz_recording_id` (sur 47 079) | 388 | **23 643** | +23 255 |
| artistes avec MBID (sur 2 696), étape B | 166 | **1 012** | +846 |
| albums avec un label | 0 | 2 102 | |
| albums avec un n° de catalogue | 0 | 1 204 | |
| albums avec un code-barres | 0 | **0** | |

La passe, album par album :

| | albums | part des 3 909 |
|---|---:|---:|
| candidats (locaux, sans MBID) | 3 909 | 100 % |
| **identifiés** | **2 099** | **53,7 %** |
| dont posés sans enregistrements (garde D : trop peu de titres concordants) | 36 | 0,9 % |
| ambigus (MusicBrainz répond, aucun pressage sûr : rien n'est écrit) | 467 | 11,9 % |
| introuvables | 1 343 | 34,4 % |
| refus du service, disjoncteur | 0 | |

- Durée : **2 h 12** (7 904 s), soit **2,02 s par album**. Les 2,77 s mesurés le
  23/09 valaient pour des albums identifiés ; un album ambigu ou introuvable
  coûte une requête de moins.
- Source des pressages : `recherche` pour les 2 099. Aucun par balise ni par
  code-barres, puisque la base n'en porte pas.
- Releases gardées en base (`musicbrainz_release_cache`) : 2 468.

## Ce que cela dit du plan

1. **Le taux réel (53,7 %) est très en dessous des prévisions** : 82 % sur
   l'échantillon de 40 albums du §4, 90 % sur le banc synthétique de #5823. Les
   deux comptaient des pressages *plausibles*. Celui-ci compte des pressages
   *posés* après la règle de confiance (étape D, MetaRust idée 1). Les 467
   albums ambigus (11,9 %) sont la part que la prudence refuse : tous résolus,
   on serait à 65,6 %.
2. **Pistes** : 23 255 pistes identifiées, contre 31 400 à 38 400 annoncées au
   §5. C'est la moitié des pistes locales.
3. **L'étape B fonctionne sans requête** : 846 artistes de plus portent un MBID
   (de 6,2 % à 37,5 %). L'étape C (passe artistes par le réseau) ne vise plus
   que le reliquat.
4. **Les crédits (étape E)** : 2 128 albums ont maintenant un pressage, et
   2 468 releases sont en base. `enrich-credits` peut les couvrir presque sans
   requête. Non mesuré ici (droit absent du banc).
5. **Les propositions Discogs (lot 2 MetaRust) ne trouveraient AUCUN album sur
   cette bibliothèque.** Après la passe, 0 album porte un code-barres. Les 1 204
   n° de catalogue sont tous sur des albums identifiés, que Discogs ne propose
   pas. La recherche Discogs par code ne sert que les bibliothèques dont les
   balises portent un code-barres ou un n° de catalogue.
6. Prochaine cible, par le nombre : les **1 343 introuvables** et les **467
   ambigus**. Le banc garde leurs identifiants dans la copie, via
   `identification_tentee_le`.

## Rejouer

```text
cp <copie de la base> <dossier de mesure>/base/copie.db   # jamais la base en service
cargo test -p tune-server --lib --no-run                  # sous un jeton Shrek
HOME=<dossier de mesure>/home \
TUNE_MESURE_A_DB=<dossier de mesure>/base/copie.db \
TUNE_MESURE_A_SORTIE=<dossier de mesure>/resultat.json \
  <binaire de test> --ignored --exact --nocapture \
  routes::library::identification_lot::mesure_a::mesure_a_sur_une_copie
```

`TUNE_MESURE_A_LIMITE=N` limite la passe aux N premiers albums de la sélection.
Le banc refuse un chemin sous `/opt/`, `/var/lib/` ou `/srv/tune/`.
