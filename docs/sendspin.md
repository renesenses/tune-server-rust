# Sendspin dans Tune

Chantier #3326. Ce document résume la spécification publique de Sendspin telle
qu'elle s'applique à Tune, décrit ce que Tune en implémente, et dit ce qui
reste. L'historique des phases précédentes (découverte, tuyau chiffré,
appairage, fragmentation) est dans
[`sendspin-protocole.md`](sendspin-protocole.md) et dans `docs/mesures/`.

## Sources et versions

Relevé du 07/10/2026, revu le 09/10/2026 contre `main` @ `ee8aad96`
(08/10/2026) : cinq clarifications depuis l'étiquette (#290, #295, #297, #298,
#304). Deux touchent le serveur et sont suivies : l'heure de réception se prend
au plus tard quand la pile WebSocket livre le message (#298, `server_received`
est désormais lu avant le déchiffrement), et un fragment tronqué est une
séquence malformée (#297, déjà refusé par `transport/fragmentation.rs`).

| Source | Version | Licence |
|---|---|---|
| Spécification, [`Sendspin/spec`](https://github.com/Sendspin/spec) | étiquette [`1.0.0-rc1`](https://github.com/Sendspin/spec/tree/1.0.0-rc1), commit `671a34d4`, publiée le 17/09/2026 | [Community Specification License 1.0](https://github.com/Sendspin/spec/blob/1.0.0-rc1/LICENSE.md) |
| Bibliothèque de référence, [`aiosendspin`](https://github.com/Sendspin/aiosendspin) | 10.0.0 sur PyPI, 05/10/2026 ; **première version publiée qui contient le chiffrement Noise** (`aiosendspin/noise/`) | Apache-2.0 |
| [`time-filter`](https://github.com/Sendspin/time-filter) (filtre de Kalman de référence, C++) | sans étiquette | Apache-2.0 |
| [`sendspin-rs`](https://github.com/Sendspin/sendspin-rs) | 0.3.7 (21/08/2026), « WIP », côté lecteur seulement | Apache-2.0 |
| [`sendspin-cpp`](https://github.com/Sendspin/sendspin-cpp) | 0.8.0 (15/09/2026), client pour matériel embarqué | Apache-2.0 |
| [`sendspin-go-server`](https://github.com/Sendspin/sendspin-go-server) | 0.1.1 (18/07/2026) | Apache-2.0 |
| [`conformance`](https://github.com/Sendspin/conformance) | sans version | non déclarée |

Les fichiers lus pour ce résumé, tous à l'étiquette `1.0.0-rc1` :
[`connection.md`](https://github.com/Sendspin/spec/blob/1.0.0-rc1/connection.md),
[`messaging.md`](https://github.com/Sendspin/spec/blob/1.0.0-rc1/messaging.md),
[`roles/player/v1.md`](https://github.com/Sendspin/spec/blob/1.0.0-rc1/roles/player/v1.md),
[`pairing.md`](https://github.com/Sendspin/spec/blob/1.0.0-rc1/pairing.md).
Attribution exigée par la licence : *Sendspin Specification, 1.0.0-rc1,
https://github.com/Sendspin/spec*. Ce qui suit est un résumé en nos mots, pas
une copie ; en cas de doute, la spécification fait foi.

La spécification reste une version candidate : `main` est à l'étiquette, mais
d'autres branches du dépôt bougent encore (poussée du 07/10/2026). Tune
s'aligne sur l'étiquette.

## Vocabulaire : Tune est le SERVEUR

Dans Sendspin, le **client** est l'appareil qui consomme (l'enceinte) et le
**serveur** est la source de musique, quel que soit le côté qui ouvre la
connexion. L'issue parle de « client lecteur » au sens de Tune qui pilote une
sortie ; dans le protocole, Tune joue le rôle **serveur** et l'enceinte le rôle
client `player@v1`.

## Le protocole en bref

### Découverte et connexion (`connection.md`)

Deux modes, et un serveur doit savoir faire les deux :

| Mode | Qui s'annonce | Service mDNS | Port recommandé | TXT |
|---|---|---|---|---|
| Initié par le serveur (recommandé) | l'enceinte | `_sendspin._tcp.local.` | 8928 | `path` requis, `name` facultatif |
| Initié par le client | le serveur | `_sendspin-server._tcp.local.` | 8927 | `path` requis, `name` facultatif |

Le transport est un WebSocket en `ws://` simple : la confidentialité vient de
Noise, à l'intérieur. Le TXT ne porte aucune identité ; l'identité d'un
appareil est sa clé publique, connue après la poignée de main. En mode initié
par le serveur, une enceinte ne garde qu'une connexion admise, classée par
l'activité déclarée (`playback` avant `pairing`, avant rien).

### Chiffrement et identités

- Noise `KKpsk2`, le serveur est l'initiateur Noise. Deux suites :
  `25519_ChaChaPoly_SHA256` et `25519_AESGCM_SHA256`, toutes deux obligatoires
  côté serveur ; le client choisit.
- `client_id` et `server_id` sont des clés publiques Curve25519 en base64url
  sans remplissage (43 caractères).
- La PSK est mêlée au second message. Trois catégories : longue durée (`lt`),
  appairage (`pr`) et Sentinelle (`sn`, constante publiée, qui chiffre sans
  authentifier). Le serveur désigne la PSK par `psk_id` dans le message 1.
- Prologue Noise : les octets exacts de `client/init` puis de `server/init`.
- Un échec de `client/init` reçoit `server/error` ; tout autre échec ferme la
  connexion sans message.

### Séquence et messages (`messaging.md`)

1. `client/init` (clair) → `server/init` (clair) → `noise/handshake` ×2 (clair).
2. Ensuite tout est binaire et chiffré ; le premier octet du clair est le type
   (`0` JSON, `1` fragment, `4`-`7` rôle `player`, …).
3. `server/hello` → `client/hello` (rôles versionnés, objet
   `player@v1_support`) → `server/activate` (`activities`, `active_roles`).
4. Juste après le premier `server/activate`, le serveur envoie `group/update`
   (`playback_state`, `group_id`, `group_name`).

Ce que la table d'activation permet : sous PSK longue durée, `[]` ou
`['playback']` ; sous Sentinelle ou PSK d'appairage, `playback` seulement si
l'enceinte a ouvert l'accès non appairé.

Un message dépassant 65 518 octets de charge se fragmente (type `1`, drapeaux
premier/dernier). Un type ou un champ inconnu s'ignore après l'activation.

### Horloge

L'enceinte envoie `client/time` ; le serveur répond `server/time` avec
`client_transmitted`, `server_received`, `server_transmitted`, en
microsecondes d'une horloge **monotone** du serveur. L'enceinte en tire, par un
filtre de Kalman à deux dimensions (décalage et dérive), la correspondance
entre horloge du serveur et la sienne. La synchronisation fine est donc à la
charge de l'enceinte : à ±1 ms (cible ±0,5 ms) en régime établi, et une vitesse
de lecture corrigée dans ±0,5 % sur 150 ms glissantes. Le serveur doit, lui,
horodater juste et envoyer assez tôt.

### Rôle `player@v1` (`roles/player/v1.md`)

- **Capacités** (`player@v1_support`) : `supported_formats` par ordre de
  préférence (`codec` `pcm`, `flac` ou `opus`, `sample_rate`, `channels`,
  `bit_depth`) et `buffer_capacity` en octets. Le serveur doit savoir produire
  `pcm` et `flac`, Opus est facultatif.
- **État** (`client/state`) : `available`, puis `volume`, `muted`,
  `output_delay_ms`, `required_lead_time_ms`, `min_buffer_ms`,
  `supported_commands`, `format` préféré. Pas de flux ni de commande avant le
  premier état ; pas de `stream/start` sans `available: true`.
- **Format** : celui que l'état préfère s'il est produisible, sinon la
  première entrée produisible de `supported_formats`.
- **Flux** : `stream/start` (ouvre ou change le format en place),
  `stream/clear` (seek et saut de piste, le flux continue), `stream/end` (fin
  réelle). Un enchaînement naturel de pistes n'envoie rien.
- **Morceau audio** (type `4`) : horodatage int64 gros-boutiste (µs, horloge
  du serveur, instant de sortie du premier échantillon), `send_ahead` uint32
  gros-boutiste (avance mesurée à l'émission, saturée à `0` et à `2³²−1`),
  puis la charge. PCM : entiers signés petit-boutistes, 24 bits sur 3 octets.
  FLAC : trames complètes, en-tête `fLaC` + STREAMINFO en base64 dans
  `codec_header`. Un morceau dure au plus 150 ms et pas moins de 15 ms (sauf
  le dernier).
- **Avance** : après un départ à vide ou un `stream/clear`, le premier morceau
  est horodaté au moins `min_buffer_ms + output_delay_ms` dans le futur, et
  vers `required_lead_time_ms` pour une source tamponnée.
- **Tampon** : la somme des morceaux non encore joués (13 octets d'en-tête
  compris) ne dépasse jamais `buffer_capacity`.
- **Commandes** (`server/command`) : `volume` (0-100, sonie perçue), `mute`,
  `set_output_delay`, seulement si elles figurent dans `supported_commands`.
- **Pas de pause** dans le protocole. Le serveur de référence n'en a pas non
  plus : il arrête le flux (`stream/end`).

## Ce que Tune implémente

| Brique | État | Où |
|---|---|---|
| Parcours `_sendspin._tcp` (découverte des enceintes) | livré (phase 1) | `tune-core/src/discovery/sendspin.rs` |
| Annonce `_sendspin-server._tcp` et point d'accès `ws://…/sendspin` | livré (S2-a) | `tune-server/src/routes/sendspin.rs` |
| Noise `KKpsk2`, deux suites, mode de transition en clair (fermé par défaut) | livré (S2-a) | `tune-core/src/sendspin/{poignee,transport,transition}.rs` |
| Appairage CPace, PSK longue durée persistées, commandes opérateur | livré (S2-b) | `tune-core/src/sendspin/{appairage,pake,magasin}.rs` |
| Fragmentation | livré | `tune-core/src/sendspin/transport/fragmentation.rs` |
| **Connexions initiées par le serveur** (Tune compose vers les `_sendspin._tcp` découverts) | **PR empilée** | `tune-server/src/routes/sendspin/{sortantes,prise}.rs` |
| **Rôle `player@v1` et sortie (zone) Sendspin** | **cette PR** | `tune-core/src/sendspin/{lecteur,horloge}.rs`, `tune-core/src/outputs/sendspin.rs`, `tune-core/src/audio/encoder.rs` (`EncodeurTramesFlac`), `tune-server/src/routes/sendspin/{pilote,zones}.rs` |

### Connexions initiées par le serveur

Le mode recommandé par la spécification, celui des enceintes qui s'annoncent
et attendent (Voice PE, ESPHome). Une boucle lancée avec le routeur relit
toutes les 2 s les annonces `_sendspin._tcp` du scanner mDNS et compose
`ws://<hôte>:<port><path>` vers chaque enceinte sans session. La suite est la
même session serveur que pour une connexion entrante (même code, via une
« prise » commune) : `client/init` de l'enceinte, Tune initiateur Noise,
appairage par la route opérateur, rôle `player@v1` et zone pour une enceinte
appairée. Reconnexion selon `client/goodbye` : coupure ou `restart` →
recomposition (2 s, doublé jusqu'à 60 s) ; `concurrent_attempt` → 60 s ;
`another_server`, `shutdown`, `user_request`, `unauthorized`,
`pairing_required`, `unpaired` → pas avant 10 min, ou au retour de l'annonce.
Une seule boucle par état de serveur. `MdnsScanner::annoncer` permet
d'annoncer une adresse à la main (réseau sans multicast, bancs).

### Le rôle `player@v1`

- **Qui obtient une zone** : une enceinte connectée à Tune (mode initié par le
  client), authentifiée par une **PSK longue durée** (donc appairée), qui
  annonce `player@v1` avec l'objet `player@v1_support` et au moins un format
  que Tune produit. Elle reçoit `active_roles: ["player@v1"]` et un
  `group/update`. Au premier `client/state`, la sortie `sendspin:<client_id>`
  est enregistrée et sa zone créée (ou remise en ligne) selon les règles des
  autres sorties réseau. À la déconnexion, la sortie est retirée et la zone
  passe hors ligne.
- **Formats** : PCM 16, 24 ou 32 bits et FLAC 16 ou 24 bits (les deux codecs
  que la spécification impose au serveur), mono ou stéréo, 8 à 384 kHz. Tune
  décode le fichier par le décodeur progressif existant, à la cadence, aux
  canaux et à la profondeur du format choisi. En FLAC, chaque morceau est UNE
  trame FLAC complète (taille de bloc fixe = la durée d'un morceau), et
  `codec_header` porte `fLaC` + STREAMINFO en Base64 standard.
- **Choix du format** : la préférence de l'enceinte (`client/state.format`)
  si Tune la produit ; sinon, la première entrée produisible au **taux natif
  de la piste** (la spécification le permet pour éviter un
  rééchantillonnage) ; sinon la première entrée produisible.
- **Cadrage** : morceaux de 50 ms (moins si `buffer_capacity` l'impose),
  horodatés sur une ligne de temps continue à partir de
  `t0 = maintenant + max(min_buffer, required_lead_time) + output_delay + 150 ms`.
  `send_ahead` est calculé juste avant le chiffrement. L'envoi se fait au plus
  1,5 s au-delà de l'avance de départ, et jamais au-delà de `buffer_capacity`.
- **Lecture** : `server/activate` `['playback']`, `group/update` `playing`,
  `stream/start`, puis l'audio. Saut de piste ou seek : `stream/clear`, le
  flux reste ouvert.
- **Une seule ligne de temps** : la piste suivante préparée par l'orchestrateur
  (`set_next_media`, la sortie déclare `can_gapless`) est posée à la suite de
  la précédente, sans `stream/clear` ni `stream/end`. Si le format change —
  taux natif de la piste suivante, ou préférence envoyée par l'enceinte en
  pleine lecture — un `stream/start` **en place** l'annonce, et le premier
  morceau du nouveau format est horodaté exactement à la fin du dernier de
  l'ancien ; le décodeur reprend à la trame qui suit la dernière envoyée (rien
  n'est renvoyé). L'état (`current_uri`, position) bascule sur la piste
  suivante quand l'horloge atteint la frontière.
- **Enceinte indisponible** (`client/state.available: false`) ou
  `client/leave` : `stream/end`, `group/update` `stopped`, activité retirée ;
  la sortie s'arrête (sans « fin naturelle ») et rien ne reprend seul.
- **Pause** (choix de cette version, à confirmer) : `stream/end` et
  `group/update` `stopped`, l'activité `playback` reste déclarée ; la reprise
  envoie un nouveau `stream/start` et repart de la position atteinte.
- **Arrêt** : `stream/end`, `group/update` `stopped`, `server/activate` `[]`.
- **Volume et sourdine** : `server/command`, volume Tune 0-1 → 0-100 ; refus
  nommé si l'enceinte ne propose pas la commande. Le volume et la sourdine
  affichés sont ceux que l'enceinte rapporte.
- **Fin de piste** : signalée quand l'horloge atteint la fin de la ligne de
  temps (pas à la fin de l'envoi), pour que la file avance au bon moment.
- **Horloge** : `server/time` vient de la même horloge monotone que les
  horodatages audio (`sendspin::horloge`).

## Ce qui reste

- **Groupes multipièces synchronisés (S2-d)** : un groupe Sendspin de
  plusieurs enceintes avec une avance commune (le maximum des avances
  individuelles), un `group_id` partagé, l'arrivée tardive d'une enceinte dans
  un flux en cours, et la porte de sortie de l'issue (deux enceintes du même
  type synchrones sur la durée d'un album). Chaque enceinte a aujourd'hui son
  groupe solo.
- **Accès non appairé** : refusé par décision (09/10/2026) ; seules les
  enceintes appairées deviennent des zones.
- `set_output_delay`, Opus (facultatif), les rôles `metadata`, `artwork`,
  `controller`, `visualizer`.
- **Source HTTP** : une URL distante est téléchargée en entier avant le
  décodage (même chemin qu'AirPlay). Le décodage progressif sur plage HTTP
  existe et serait le bon branchement.
- **Matériel réel** : le rôle est validé contre un pair simulé écrit d'après
  la spécification ET contre le lecteur de référence `aiosendspin` 10.0.0
  (`tune-server/tests/sendspin/lecteur_aiosendspin_3326.rs`, `#[ignore]`,
  `--ignored` avec `TUNE_AIOSENDSPIN_PYTHON`), pas encore contre une enceinte. Le banc ne
  mesure pas la précision de sortie (±1 ms), qui est à la charge du lecteur et
  se mesure au DAC.

## Banc d'interopérabilité aiosendspin

```sh
uv venv /chemin/venv && uv pip install -p /chemin/venv/bin/python aiosendspin soundfile
TUNE_AIOSENDSPIN_PYTHON=/chemin/venv/bin/python TUNE_AIOSENDSPIN_DOSSIER=/tmp/banc \
  cargo test -p tune-server --test sendspin_point_d_acces_s2a -- aiosendspin --ignored --nocapture
```

Le script `tests/sendspin/banc_aiosendspin.py` fait d'aiosendspin un lecteur
qui enregistre au lieu de jouer ; il s'appaire par « Pairing PSK » via la
route opérateur. Le test vérifie la séquence de contrôle vue par le lecteur,
la ligne de temps (continue dans chaque flux et à travers les `stream/start`
en place), l'avance de chaque morceau sur l'heure de lecture prédite par le
filtre de Kalman d'aiosendspin, le contenu bit à bit (PCM tel quel, FLAC
décodé par libsndfile, pas par Tune), et l'erreur du filtre de temps (< 1 ms).
Les journaux des deux côtés restent dans `TUNE_AIOSENDSPIN_DOSSIER`.
