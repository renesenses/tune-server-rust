# Pont Roon : récolter son Core Roon et l'importer dans Tune

Le Pont Roon apporte à Tune ce que votre Core Roon connaît déjà : les **crédits
par piste** (compositeurs, auteurs), les **images d'artistes** et les
**pochettes d'albums**. Il se fait en deux temps :

1. le **moissonneur** (`moissonneur-roon`), un petit programme en ligne de
   commande, lit votre Core Roon et range tout dans **une archive** `.zip` ;
2. l'**extension Pont Roon** de Tune importe cette archive.

Le moissonneur est en **lecture seule** : il ne lance aucune lecture et ne
modifie aucun réglage de Roon.

## Prérequis

- Un compte Tune **Premium** : sans lui, l'import est refusé (réponse
  `402 premium_required`).
- L'extension **Pont Roon** installée et activée dans Tune (Tune v0.9.152 ou
  plus récent). Elle est désactivée par défaut.
- Une machine qui **voit le Core Roon sur le réseau** pour lancer le
  moissonneur : le Mac, le PC ou la machine Linux du Core lui-même, ou une
  autre machine du même réseau. Il n'a pas besoin de tourner à côté de Tune.
- **Aucune adresse IP à connaître** : le moissonneur cherche le Core tout seul
  sur le réseau. Gardez-la quand même sous la main pour les cas où la diffusion
  ne passe pas (Wi-Fi avec isolation des clients, VLAN, Docker) : elle se donne
  alors avec `--hote=`.

## 1. Télécharger le moissonneur

Le moissonneur a **sa propre release**, séparée de celle du serveur : cherchez
`Moissonneur Roon v0.9.x` dans la
[liste des releases](https://github.com/renesenses/tune-server-rust/releases),
et non pas les archives du serveur.

Pourquoi à part : quand ces archives voyageaient sur la release de Tune, la
mise à jour automatique du serveur y prenait `moissonneur-roon-…` pour
`tune-server-…` et échouait. La séparation est ce qui l'en empêche — une
release du moissonneur ne contient aucun fichier `tune-server…`, donc aucun
serveur ne la consultera jamais.

| système | archive |
|---|---|
| macOS Apple Silicon | `moissonneur-roon-<version>-macos-arm64.tar.gz` |
| macOS Intel | `moissonneur-roon-<version>-macos-x86_64.tar.gz` |
| Linux x86_64 | `moissonneur-roon-<version>-linux-x86_64.tar.gz` |
| Linux ARM 64 bits | `moissonneur-roon-<version>-linux-aarch64.tar.gz` |
| Windows x86_64 | `moissonneur-roon-<version>-windows-x86_64.zip` |

Les binaires Linux sont statiques (musl) : aucune bibliothèque à installer.

Décompressez l'archive dans un dossier de votre choix, puis ouvrez un terminal
dans ce dossier.

**macOS** : le binaire est signé, mais pas notarisé. Un fichier téléchargé par
le navigateur porte l'attribut de quarantaine et macOS refusera de le lancer.
Retirez-le une fois :

```sh
xattr -d com.apple.quarantine ./moissonneur-roon
```

## 2. Lancer le moissonneur

```sh
./moissonneur-roon --archive=export-roon.zip
```

(sous Windows : `moissonneur-roon.exe --archive=export-roon.zip`)

**Aucune adresse à taper** : le moissonneur interroge le réseau et affiche le
Core qu'il a trouvé.

```
recherche du Core Roon sur le réseau (5 s)…
Core trouvé : « Salon » — 192.168.1.20:9330 (Roon 2.65)
```

Deux cas où il vous rend la main plutôt que de choisir :

- **plusieurs Cores répondent** — il les liste tous et attend que vous disiez
  lequel avec `--hote=` ;
- **aucun ne répond** — la diffusion ne traverse pas toujours : Wi-Fi avec
  « isolation des clients », VLAN séparé, conteneur Docker sans
  `--network=host`. Donnez alors l'adresse à la main :

  ```sh
  ./moissonneur-roon --hote=192.168.1.20 --archive=export-roon.zip
  ```

  Sur un réseau lent, ou un Core qui vient de démarrer, essayez d'abord
  `--decouverte=20`.

Options :

| option | rôle | défaut |
|---|---|---|
| `--hote=<ip>` | adresse du Core Roon — **facultative**, elle court-circuite la recherche | le Core trouvé sur le réseau |
| `--port=<port>` | port du Core | celui qu'annonce le Core, sinon `9330` |
| `--decouverte=<secondes>` | combien de temps écouter le réseau | `5` |
| `--archive=<fichier.zip>` | écrit l'archive à importer (export + octets des images) | pas d'archive |
| `--sortie=<fichier.json>` | fichier `export.json` écrit à côté | `export-roon.json` |
| `--sans-pistes` | ne descend pas dans les albums : ni pistes, ni crédits | pistes récoltées |
| `--jeton=<fichier>` | où garder l'autorisation Roon (voir §3) | dossier de configuration du compte |
| `--aide` | rappelle tout ceci et s'arrête | — |

Les options s'écrivent **avec `=`** (`--hote=192.168.1.20`, pas
`--hote 192.168.1.20`).

**Utilisez toujours `--archive`.** Sans elle, l'export ne contient que les
*clés* des images de Roon, qui ne servent qu'au Core qui les a émises : Tune
ne pourrait poser aucune image.

## 3. Autoriser l'extension dans Roon

**Une seule fois.** Au tout premier lancement, le moissonneur affiche :

```
connexion à 192.168.1.20:9330 — autorisez « Tune — moissonneur » dans Roon (Réglages → Extensions)
(une seule fois : l'autorisation est gardée dans …/tune-moissonneur-roon/jeton-roon.json)
```

Dans Roon, ouvrez **Réglages → Extensions** et **activez « Tune —
moissonneur »** (éditeur : Mozaik Labs). Le moissonneur attend cette
autorisation, puis affiche `connecté.` et commence.

Aux lancements suivants, il ne vous redemande plus rien :

```
connexion à 192.168.1.20:9330 — autorisation déjà donnée (jeton : …/jeton-roon.json)
```

Le jeton est rangé dans le dossier de configuration de **votre compte**, pas
dans le dossier d'où vous lancez la commande :

| système | fichier |
|---|---|
| macOS | `~/Library/Application Support/tune-moissonneur-roon/jeton-roon.json` |
| Linux | `~/.config/tune-moissonneur-roon/jeton-roon.json` (ou `$XDG_CONFIG_HOME`) |
| Windows | `%APPDATA%\tune-moissonneur-roon\jeton-roon.json` |

`--jeton=<fichier>` le range ailleurs (clé USB, machine partagée).

Ce fichier **autorise à lire votre Core Roon** : sur macOS et Linux il n'est
lisible que par vous (`600`), dans un dossier `700`. Le moissonneur n'en
affiche jamais le contenu, seulement le chemin. Traitez-le comme un mot de
passe : ne le recopiez pas dans un ticket ni sur le forum.

**Pour retirer l'autorisation**, au choix :

- **supprimer le fichier** — le moissonneur redemandera l'autorisation au
  lancement suivant ;
- **dans Roon**, Réglages → Extensions → « Tune — moissonneur » → désactiver
  ou oublier l'extension. C'est le seul des deux gestes qui coupe l'accès
  côté Core ; faites-le si la machine du moissonneur vous échappe.

## 4. Où est l'archive

La récolte liste d'abord les artistes, puis parcourt leurs albums et pistes,
puis télécharge les images (JPEG, 1 200 px au plus) :

```
images : <n> reçues sur <n> clés (<n> échecs, <n> Mo)
ARCHIVE export-roon.zip
ÉCRIT export-roon.json — <n> artistes, <n> albums, <n> pistes, en <durée>
```

Deux fichiers sont écrits **dans le dossier où vous avez lancé la commande**
(ou au chemin donné à `--archive` / `--sortie`) :

- `export-roon.zip` — **le fichier à importer dans Tune** : `export.json` et
  `images/<clé>.jpg` ;
- `export-roon.json` — le même export, sans les images, lisible à l'œil.

Ce que l'API de Roon ne fournit pas n'est pas récolté, et l'export le dit
(`absent_de_l_api`) : biographies, artistes similaires, suggestions de
découverte, identifiants externes (MBID, UPC, ISRC).

## 5. Importer dans Tune

Dans Tune : **Extensions → Pont Roon**.

1. Déposez `export-roon.zip`.
2. Lancez d'abord l'**aperçu** : Tune compte ce qu'il ferait **sans rien
   écrire**.
3. Si le rapport vous convient, lancez l'**import**.

Le rapport du dernier import (hors aperçu) est conservé et réaffiché sur
l'écran de l'extension.

L'archive ne doit pas dépasser **8 Gio**. Tune l'écrit sur disque au fil de
l'envoi : au-delà du plafond, il répond une erreur lisible (`413`) au lieu de
couper la connexion en plein milieu. Un `export-roon.json` seul est accepté
aussi, mais il n'apporte alors que les crédits, aucune image.

## Ce que fait l'import, et ce qu'il ne fait jamais

L'appariement se fait sur **vos artistes, puis leurs albums, puis leurs
pistes**, par nom et titre.

Ce qu'il fait :

- **crédits** : écrits seulement sur une piste de Tune qui n'a **aucun**
  crédit ; rôle « compositeur », marqués comme venant de Roon ;
- **image d'artiste** : posée seulement sur un artiste qui **n'a pas
  d'image** dans Tune ; marquée comme venant de Roon ;
- **pochette** : posée seulement sur un album qui **n'a pas de pochette**.

Ce qu'il ne fait jamais :

- il ne **remplace aucune image** ni aucune pochette existante, qu'elle ait été
  choisie, scannée ou enrichie ;
- il ne **remplace aucun crédit** existant ;
- il ne crée ni artiste, ni album, ni piste : ce que Tune ne connaît pas est
  listé dans le rapport (`artistes_inconnus`, `albums_inconnus`) et ignoré ;
- ce qui vient de Roon **reste local** : la synchronisation cloud de Tune ne
  l'emporte pas.

Le rapport donne, entre autres : artistes, albums et pistes appariés ;
crédits à écrire, déjà présents, écrits ; images nommées par l'export et
portées par l'archive ; images d'artistes et pochettes à poser, et posées.

## Annexe : import en ligne de commande

En attendant l'écran d'import, ou pour l'automatiser, les deux routes de
l'extension :

```sh
# État : droit Premium, dernier rapport
curl http://<serveur-tune>:8888/api/v1/ext/pont-roon/

# Aperçu : compte, n'écrit rien
curl -X POST --data-binary @export-roon.zip \
  "http://<serveur-tune>:8888/api/v1/ext/pont-roon/import?apercu=true"

# Import
curl -X POST --data-binary @export-roon.zip \
  "http://<serveur-tune>:8888/api/v1/ext/pont-roon/import?apercu=false"
```

Si l'authentification est activée sur votre serveur Tune, ajoutez
`-H "Authorization: Bearer <jeton>"` à chaque commande.
