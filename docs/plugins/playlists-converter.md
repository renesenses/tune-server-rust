# Greffon « Playlists converter »

Tranches 2 et 3 de l'épique [#4715](https://github.com/renesenses/tune-server-rust/issues/4715)
([#4717](https://github.com/renesenses/tune-server-rust/issues/4717),
[#4718](https://github.com/renesenses/tune-server-rust/issues/4718)). Transfère
une playlist d'un service vers un autre, **à l'identique** et **par lot**, et
garde une **copie datée** (snapshot) de toute playlist avant d'y écrire.

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
| `POST /snapshot` | `{service, playlist_id, nom?}` | `{snapshot: en-tête}` — lecture seule chez le service |
| `GET /snapshots` | — | `{count, playlists: [{service, playlist_id, nom, snapshots, dernier_le_ms}], retention_par_playlist}` |
| `GET /snapshots?service=S&playlist_id=P` | — | `{count, snapshots: [en-tête…], retention_par_playlist}`, du plus récent au plus ancien |
| `GET /snapshot?id=snap-K-N` | — | `{snapshot: {…en-tête, pistes: [{id, titre, artiste, duree_ms, isrc}]}}` ; `404` inconnu ou expiré |
| `POST /snapshot/restauration/apercu` | `{snapshot_id, mode: "completer"\|"recreer"}` | `{plan, a_rajouter: [piste…], a_retirer_par_vous: [piste…]}` — **rien n'est écrit** |
| `POST /snapshot/restauration` | `{plan_id, accord: true}` | `{plan, a_retirer_par_vous}` ; `409` sans accord ou plan déjà exécuté |

Un en-tête de snapshot : `{snapshot_id, service, playlist_id, nom, pris_le_ms,
motif, total, pages, empreinte}`. `motif` vaut `manuel`,
`avant_transfert:lot-N` ou `avant_restauration:plan-N`.

`source_service` vaut `"local"` pour la bibliothèque (les `playlists` sont
alors des identifiants entiers en texte). Sans `suffixe_nom`, le nom de la
playlist créée est **repris à l'identique**.

## Snapshots (#4718)

Avant **tout** transfert, le greffon garde une copie datée de la playlist
visée : son nom, ses pistes et leurs identifiants de service, avec l'heure de
l'hôte (`host_now`). Le transfert n'écrit **aucun** titre tant que cette copie
n'est pas écrite ; son identifiant est rangé dans le lot
(`lot.playlists[i].snapshot_avant`). Une playlist que le transfert vient de
créer est gardée vide — c'est son état d'avant.

On peut aussi prendre un snapshot à la main de n'importe quelle playlist
(service ou bibliothèque locale), les lister et en lire le contenu.

### Le retour en arrière ne supprime RIEN

L'interface hôte n'a **aucune** capacité de suppression, et ce n'est pas un
manque à combler : c'est la règle. Un retour en arrière est donc **non
destructif**, et se fait toujours en deux temps — **aperçu**, puis **accord** :

| Mode | Ce qui est fait | Ce qui ne l'est pas |
|---|---|---|
| `completer` (défaut) | les pistes du snapshot qui ont disparu de la playlist y sont **rajoutées** (en fin de playlist) | les pistes ajoutées depuis le snapshot **restent** : elles sont listées dans `a_retirer_par_vous`, et c'est à **l'utilisateur** de les retirer lui-même, depuis l'application du service, s'il le souhaite. L'ordre d'origine n'est pas rétabli. |
| `recreer` | une **nouvelle** playlist est créée avec le nom et les pistes du snapshot | l'ancienne playlist n'est ni modifiée ni supprimée |

Le retour en arrière n'écrit jamais **plus** que l'aperçu accepté (une piste
disparue après l'aperçu attendra un nouvel aperçu), et en mode `completer` il
prend lui-même un snapshot de l'état courant avant d'écrire : il se défait
comme le reste.

### Rétention

Le stockage clé/valeur ne sait pas non plus supprimer une clé : la rétention
est un **anneau**.

* **10 snapshots par playlist.** Le onzième réécrit le plus ancien ; le
  demander ensuite rend `404 snapshot_expire`.
* Un snapshot **identique** au précédent (même nom, mêmes pistes, même ordre)
  n'occupe pas d'emplacement : le précédent est rendu.
* Les pistes sont rangées par pages de 400, pour tenir sous la borne de
  256 Kio par valeur.
* **20 plans de retour en arrière** gardés, tous confondus.

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
| `lot:<id>:pl:<rang>` | une playlist : appariées, introuvables, identifiant cible, versées, snapshot d'avant |
| `compteur_playlists_snap` | le dernier numéro de playlist ayant un snapshot |
| `snap_pl:<service>/<playlist_id>` | le registre d'une playlist : son numéro `K`, son nom, le nombre de snapshots pris |
| `snap:<K>:<emplacement>` | l'en-tête d'un snapshot (anneau de 10) |
| `snap:<K>:<emplacement>:p:<n>` | une page de 400 pistes |
| `compteur_restaurations`, `restauration:<emplacement>` | les plans de retour en arrière (anneau de 20) |

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
| `cargo test -p tune-playlists-converter` | le moteur en natif, contre un hôte de banc : règle des ±3 s, aperçu sans écriture, rapport, reprise, mode par lot, snapshots, rétention, retour en arrière |
| `cargo test -p tune-server --features plugins-wasm --test greffon_convertisseur_4717` | le **vrai `main.wasm`** dans le vrai bac à sable : l'ABI, les permissions, et les mêmes garanties bout en bout |
| `cargo test -p tune-server --features plugins-wasm --test greffon_snapshots_4718` | le vrai `main.wasm` : snapshot écrit AVANT le premier titre versé, retour en arrière qui rajoute sans rien supprimer, date lue par `host_now` |

Le second est le seul à exercer `src/abi.rs` (allocation, empaquetage
`(ptr << 32) | len`, imports `"tune"`).
