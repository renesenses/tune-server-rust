# Greffon « Playlists converter »

Tranche 2 de l'épique [#4715](https://github.com/renesenses/tune-server-rust/issues/4715)
([#4717](https://github.com/renesenses/tune-server-rust/issues/4717)). Transfère
une playlist d'un service vers un autre, **à l'identique** et **par lot**.

Greffon **WASM**, **premium** (`manifest.premium = true`) et **facultatif** :
il n'est pas embarqué dans les paquets publiés, il s'installe depuis le
gestionnaire de greffons. Source : `plugins/tune-playlists-converter`.

## Les trois garanties

| Garantie | Comment elle tient |
|---|---|
| **Aucune écriture sans aperçu ni accord** | `POST /transfert` exige un `lot_id` produit par `POST /apercu` **et** `accord: true`. L'aperçu n'appelle aucune capacité d'écriture — il n'en a pas le chemin. |
| **Rapport par playlist** | chaque playlist porte ses `appariees` et ses `introuvables`, chaque introuvable portant sa `raison` (un code + le détail mesuré). |
| **Reprise sans doublon** | l'identifiant de la playlist créée et les identifiants déjà versés sont écrits dans le stockage clé/valeur **avant** l'étape suivante. Une reprise ne recrée rien et ne reverse rien. |

## La règle d'appariement (Bertrand, 22/09/2026)

Quand l'identifiant de service ne correspond pas : **titre + artiste + durée à
±3 secondes**. Les trois doivent concorder.

* Le verdict **titre + artiste** n'est pas réécrit : c'est celui du matcher
  partagé du projet (`tune_core::streaming::matching`), relayé par la fonction
  hôte `host_streaming_match_track` qui rend `score` et `approximate`.
* La **durée** est le critère que le greffon ajoute. Tolérance inclusive de
  3 000 ms ; au-delà, introuvable, avec l'écart mesuré.
* Une **durée absente** d'un côté ou de l'autre vaut introuvable : le troisième
  critère n'a pas pu être vérifié, donc il n'est pas tenu.

Un remaster ou une autre édition ressort donc en `duree_hors_tolerance`. C'est
voulu : mieux vaut un manque qu'un faux transfert.

### Les raisons

| Code | Sens |
|---|---|
| `aucun_resultat` | le service n'a rien rendu pour ce titre |
| `appariement_approximatif` | titre/artiste sous le seuil d'acceptation (le `score` est joint) |
| `duree_hors_tolerance` | titre et artiste concordent, la durée non (`ecart_ms` joint) |
| `duree_inconnue` | une des deux durées manque |
| `service_en_erreur` | l'appel a échoué ; le message de l'hôte est conservé |

## Routes

Montées par l'hôte sous `/api/v1/plugins/playlists-converter/…`. La garde
premium est posée **par l'hôte** avant le dispatch : le greffon ne vérifie
aucune licence.

| Route | Corps | Réponse |
|---|---|---|
| `POST /apercu` | `{source_service, cible_service, playlists: [id…], suffixe_nom?}` | `{resume, lot}` — **rien n'est écrit** |
| `POST /transfert` | `{lot_id, accord: true}` | `{resume, lot}` ; `409` sans accord ou sur un lot déjà engagé |
| `POST /reprise` | `{lot_id}` | `{resume, lot}` ; `409` si le lot n'a jamais été accepté |
| `GET /lots` | — | `{count, lots: [en-tête…]}` |
| `GET /lot?id=lot-N` | — | `{resume, lot}` |

`source_service` vaut `"local"` pour la bibliothèque (les `playlists` sont
alors des identifiants entiers en texte). Sans `suffixe_nom`, le nom de la
playlist créée est **repris à l'identique**.

## Ce que le greffon ne peut pas faire

* **Supprimer quoi que ce soit.** Aucune capacité de suppression n'existe dans
  l'interface hôte : ni playlist, ni piste, ni chez un service. Une capacité
  absente est la seule garde qu'on ne contourne pas.
* **Parler HTTP.** Pas de permission `net`. L'ETag exigé par TIDAL pour
  modifier une playlist et la pagination des pistes sont l'affaire du
  connecteur, derrière `add_tracks_to_playlist` / `get_playlist_tracks`. Le
  greffon verse par paquets de 100 — la taille d'un lot TIDAL, et le grain de
  la reprise.
* **Écrire DANS la bibliothèque locale.** Il faudrait apparier un titre dans la
  bibliothèque, et la tranche 1 n'expose aucune capacité de recherche locale
  (`host_search` / un `host_library_match_track`, permission `library`). La
  demande est **refusée explicitement** (`cible_locale_non_supportee`) plutôt
  que silencieusement approximée. Le sens inverse — bibliothèque → service —
  fonctionne.

## Stockage

Dans le stockage clé/valeur cloisonné du greffon (#4716), donc sous
`plugin_kv:playlists-converter:` côté `settings` :

| Clé | Contenu |
|---|---|
| `compteur_lots` | le dernier numéro attribué |
| `lot:<id>` | l'en-tête : services, état, rangs des playlists |
| `lot:<id>:pl:<rang>` | une playlist : appariées, introuvables, identifiant cible, versées |

Une playlist par clé **à dessein** : l'hôte borne une valeur à 256 Kio, et un
lot de trente playlists de trois cents titres n'y tiendrait pas d'un bloc.

## Construire le `main.wasm`

```sh
cd plugins/tune-playlists-converter
RUSTFLAGS="-C opt-level=z -C codegen-units=1 -C panic=abort -C strip=symbols" \
  cargo build --target wasm32-unknown-unknown --release --lib
cp "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/tune_playlists_converter.wasm" \
   ../../tune-server/tests/fixtures/plugins/playlists-converter/main.wasm
```

Le `--lib` n'est pas décoratif : `lto` refuse une caisse `rlib`, et la caisse
porte `rlib` **en plus** de `cdylib` pour que `cargo test` puisse la lier.

## Où se jouent les preuves

| Porte | Ce qu'elle couvre |
|---|---|
| `cargo test -p tune-playlists-converter` | le moteur en natif, contre un hôte de banc : règle des ±3 s, aperçu sans écriture, rapport, reprise, mode par lot |
| `cargo test -p tune-server --features plugins-wasm --test greffon_convertisseur_4717` | le **vrai `main.wasm`** dans le vrai bac à sable : l'ABI, les permissions, et les mêmes garanties bout en bout |

Le second est le seul à exercer `src/abi.rs` (allocation, empaquetage
`(ptr << 32) | len`, imports `"tune"`).
