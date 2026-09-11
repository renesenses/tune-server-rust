# Sendspin — ce que le protocole demande à Tune

Relevé le 08/09/2026 sur `https://github.com/Sendspin/spec`, commit `e0a28529`
(07/09/2026). Chantier #3326, phase 1. Tout ce qui suit est cité de la
spécification ou vérifié par requête ; rien n'est déduit.

> **Le dépôt de spécification ne porte AUCUNE licence.**
> `https://api.github.com/repos/Sendspin/spec/license` → `404`,
> `"license": null` sur le dépôt, aucun fichier `LICENSE` à la racine.
> Les implémentations, elles, sont licenciées (voir § Écosystème).
> Ce document est une NOTE DE LECTURE : il décrit ce que la spécification
> demande, il n'en recopie pas le texte.

## 0. Le renversement de vocabulaire, à lire en premier

Dans Sendspin, **le « client » est l'enceinte** et **le « serveur » est la
source de musique** :

> « The Sendspin client is always the consumer of data like audio or metadata,
> regardless of who initiated the connection. » — `connection.md`

Conséquence pour Tune : pour envoyer de la musique vers une enceinte Sendspin,
**Tune doit implémenter le rôle SERVEUR** du protocole. Le ticket #3326 parle de
« client lecteur » au sens de Tune-qui-pilote-une-sortie ; c'est l'intention
produit juste, mais le vocabulaire protocolaire inverse. Ce n'est pas un détail
de nommage : le rôle serveur est le plus chargé des deux (voir § 6).

## 1. Découverte

Deux modes, `connection.md` § *Establishing a Connection*. « Servers must
support both methods described below. »

| Mode | Qui s'annonce | Service mDNS | Port recommandé | TXT |
|---|---|---|---|---|
| Serveur-initié (**recommandé**) | le LECTEUR | `_sendspin._tcp.local.` | `8928` | `path` **REQUIS**, `name` facultatif |
| Client-initié | le SERVEUR | `_sendspin-server._tcp.local.` | `8927` | `path` **REQUIS**, `name` facultatif |

- `path` est le chemin du point d'accès WebSocket, valeur recommandée
  `/sendspin`. Il est REQUIS : **aucun défaut ne doit être inventé**.
- `name` n'est qu'une « discovery-time hint » ; en cas de désaccord, c'est le
  `client/hello` (ou `server/hello`) qui fait foi.
- **Il n'y a aucun autre TXT.** Pas de version, pas de rôles, pas
  d'identifiant.

L'identité durable d'un lecteur est son `client_id` : sa clé publique
Curve25519, base64url sans remplissage, 43 caractères — connue seulement APRÈS
la poignée de main Noise (`connection.md` § *Identities*).

Arbitrage multi-serveurs (mode serveur-initié) : un lecteur ne tient qu'UNE
connexion admise à la fois, classée par l'activité déclarée dans
`server/activate` (`'playback'` > `'pairing'` > vide). Une connexion provisoire
sans `server/activate` sous 30 s est abandonnée.

## 2. Transport et chiffrement

- **WebSocket sur TCP, en `ws://` obligatoirement** : « The WebSocket transport
  MUST be plain `ws://`. Confidentiality and integrity are provided end to end
  by the Noise layer inside the WebSocket payloads. »
- **Noise `KKpsk2`**, clés statiques connues des deux côtés, PSK mêlée au second
  message. **Le serveur est l'initiateur Noise**, le client le répondeur, quel
  que soit le sens TCP.
- Deux suites : `25519_ChaChaPoly_SHA256` et `25519_AESGCM_SHA256`. **Un serveur
  doit supporter les deux** ; un client au moins une. Le client choisit dans
  `client/init`, sans négociation.
- Trois catégories de PSK : `'lt'` (long terme), `'pr'` (appairage), `'sn'`
  (Sentinel — constante publiée, qui n'authentifie rien par elle-même).
- Cadrage : les trois messages de poignée de main (`client/init`, `server/init`,
  `noise/handshake`) sont des trames **texte** JSON en clair ; ensuite tout
  passe en trames **binaires**, chaque message étant un chiffré Noise dont le
  premier octet déchiffré est le type.

## 3. Types de messages binaires

`messaging.md` :

| Type | Attribution |
|---|---|
| 0 | corps JSON (UTF-8) |
| 1 | fragmentation |
| 2 | appairage (clip audio de chiffres) |
| 4-7 | rôle `player` |
| 8-11 | rôle `artwork` |
| 12-15 | rôle `source` |
| 16-23 | rôle `visualizer` |
| 192-255 | rôles spécifiques à une application |

Un message Noise est plafonné à 65535 octets ; moins le tag AEAD (16) et l'octet
de type, la charge utile par trame est de **65518 octets**. Au-delà :
fragmentation (type 1), un seul message fragmenté en vol par direction.

## 4. Séquence d'établissement

1. client → serveur `client/init` — `client_id`, `version` (= 1), `suite`
2. serveur → client `server/init` — `server_id`, `version`
3. serveur → client `noise/handshake` (message Noise 1)
4. client → serveur `noise/handshake` (message Noise 2)
5. bascule en transport Noise (trames binaires)
6. serveur → client `server/hello` — `name`, `languages?`
7. client → serveur `client/hello` — `name`, `device_info?`,
   `supported_roles` (versionnés : `player@v1`, …), `<rôle>_support`,
   `supported_pair_methods`, `unpaired_access`
8. serveur → client `server/activate` — `activities`
   (`'playback'` / `'pairing'`, éventuellement vide), `active_roles?`,
   `pairing?`

Puis, en régime : `client/time` / `server/time`, `stream/start`, et les trames
audio.

## 5. Rôle `player` — le flux

`roles/player/v1.md`.

- Codecs : `'opus'`, `'flac'`, `'pcm'`. **« Servers MUST support all audio
  codecs »** — les trois, pas un choix. PCM en entiers signés petit-boutistes,
  24 bits sur 3 octets.
- En-tête d'une trame audio (type 4) :
  - octet 0 : type `4`
  - octets 1-8 : `timestamp`, int64 gros-boutiste, microsecondes, dans le
    domaine d'horloge du SERVEUR
  - octets 9-12 : `send_ahead`, uint32 gros-boutiste ; ne porte **aucune**
    sémantique d'ordonnancement, sert seulement au lecteur à mesurer son retard
  - la suite : la trame encodée
- Durée d'un morceau : **≤ 150 ms**, et « SHOULD NOT » < 15 ms.
- Commandes serveur → lecteur (`server/command`, objet `player`) :
  `'volume'` (0-100), `'mute'`, `'set_output_delay'` (0-5000 ms). Le volume est
  une **sonie perçue** : « `amplitude = (volume / 100)^1.5` ». Volume et
  sourdine sont indépendants.
- Cycle de vie du flux : `stream/start` (démarre ou met à jour la
  configuration en place), `stream/clear` (vide les tampons — c'est le
  mécanisme du **seek ET du saut de piste**), `stream/end` (fin réelle). Une
  transition naturelle entre deux pistes **n'envoie rien** : le flux continue,
  c'est ce qui rend le sans-blanc et le fondu enchaîné possibles. Envoyer
  `stream/end` dans ce cas est « explicitly prohibited ».

## 6. Synchronisation

`messaging.md` § *Clock Synchronization* et `roles/player/v1.md` §
*Correction Quality* :

- « The time filter is a two-dimensional Kalman filter that tracks both clock
  offset and drift. » Son usage est **obligatoire** côté lecteur.
- « The effective playback speed MUST stay within ±0.5% of normal speed,
  measured as a sliding average over 150 ms. » — c'est la fenêtre de mesure de
  la déviation de vitesse, à ne pas confondre avec la durée maximale d'un
  morceau audio, qui vaut 150 ms elle aussi.
- « In steady state, implementations MUST keep this error within ±1 ms »
  (cible recommandée ±0,5 ms), mesurée **contre ce que le filtre prédit**, pas
  contre l'horloge serveur réelle.
- Implémentation de référence C++ : `https://github.com/Sendspin/time-filter`
  (Apache-2.0). L'organisation `Sendspin-Protocol` redirige vers `Sendspin`.

Ce que cela impose au **serveur** : tenir une horloge monotone en microsecondes,
répondre aux `client/time` avec `client_transmitted` / `server_received` /
`server_transmitted`, et ordonnancer chaque morceau assez en avance pour
satisfaire le `required_lead_time_ms` et le `min_buffer_ms` que chaque lecteur
publie dans son `client/state`.

## 7. Remontée d'état

Un seul message montant : `client/state`, avec `available` (obligatoire à chaque
envoi) et l'état complet de chaque objet de rôle inclus. L'objet `player` porte
`volume?`, `muted?`, `output_delay_ms`, `required_lead_time_ms`,
`min_buffer_ms`, `supported_commands`, `format?`.

**Il n'existe aucun message d'erreur applicatif.** Les erreurs de poignée de
main ferment le WebSocket « without sending any application-level error
message ». La seule sortie propre est `client/goodbye` avec un motif fermé
(`another_server`, `shutdown`, `restart`, `user_request`, `unauthorized`,
`pairing_required`, `concurrent_attempt`, `unpaired`).

## 8. Rôles

Sept, pas trois : `player`, `source`, `controller`, `metadata`, `artwork`,
`visualizer`, `color`. Versionnés (`player@v1`) ; le serveur active au plus une
version par famille.

Pour Tune, `player` suffit à faire du son. `controller` est celui qui permettrait
à une télécommande Sendspin de piloter Tune — hors périmètre.

## 9. Écosystème et licences (vérifiés le 08/09/2026)

| Dépôt | Licence déclarée par l'API GitHub |
|---|---|
| `Sendspin/spec` | **aucune** (`license: null`, `/license` → 404) |
| `sendspin-rs`, `time-filter`, `aiosendspin`, `sendspin-go`, `sendspin-go-server`, `sendspin-cpp`, `sendspin-js`, `SendspinKit` | Apache-2.0 |
| `sendspin-dotnet` | MIT |
| `sendspin-jvm`, `sync-test` | NOASSERTION |
| `conformance`, `sendspin-vst`, `backlog`, `audio-sdk-js` | aucune |

### `sendspin-rs` — pourquoi la caisse ne nous sert pas

- Nom du crate : `sendspin`, version `0.3.7` sur crates.io (13 versions,
  1338 téléchargements, dernière publication 21/08/2026).
- Manifeste : `license = "MIT OR Apache-2.0"` ; **le dépôt ne contient qu'un
  fichier `LICENSE` Apache-2.0**, aucun texte MIT. Incohérence de conditionnement
  à signaler en amont si nous devions en dépendre.
- Description du dépôt : « Sendspin Rust Libary (WIP) » ; README : « ⚠️ THIS IS
  A WIP », « Phase 2: Audio Pipeline 🚧 (Next) ».
- **C'est une bibliothèque CLIENT (lecteur)** : `src/audio/decode/{flac,opus,pcm}`,
  `cpal`, `synced_player.rs`. Il n'y a **aucun module serveur**.
- `src/protocol/messages.rs` ne connaît ni `client/init`, ni `server/init`, ni
  `noise/handshake`, ni `server/activate` ; aucune dépendance cryptographique
  (`snow`, `x25519`) au manifeste. Elle implémente donc le Sendspin **antérieur
  au chiffrement**.
- La découverte mDNS n'y est pas : `mdns-sd` est en `[dev-dependencies]`,
  utilisé seulement par `examples/server_initiated_metadata.rs`.

Conclusion : **`sendspin-rs` ne couvre pas le besoin de Tune** (le serveur), ni
dans son état actuel le protocole courant. Le seul serveur de l'organisation est
`sendspin-go-server` (Go, Apache-2.0) — lisible comme référence, pas
importable.

## 10. Ce que Tune livre en phase 1

- `tune-core/src/discovery/sendspin.rs` : constantes de service et de port,
  lecture des deux TXT, construction de l'URL WebSocket, description JSON.
- `OutputType::Sendspin`, de priorité `0` — sous tout protocole qui joue, pour
  qu'une annonce Sendspin ne prenne jamais la place d'une sortie fonctionnelle
  au dédoublonnage.
- Parcours mDNS de `_sendspin._tcp.local.`, **sans enregistrer aucune sortie** :
  aucune zone Sendspin ne peut naître.
- `GET /devices/sendspin` : la liste, avec `playable: false` et le motif.

Et ce qu'elle ne livre pas, volontairement : la poignée de main Noise,
l'appairage, l'encodage, l'horloge, la lecture.

## 11. Corrections mesurées sur le fil (09/09/2026, brique S2-a)

Ce qui suit ne vient pas d'une relecture de la spécification mais d'échanges
**réellement observés** entre le serveur de Tune et l'implémentation de
référence de l'Open Home Foundation (`aiosendspin`). Là où ces constats
contredisent les sections 1 à 10 ci-dessus, **c'est le fil qui fait foi**.

### 11.1 La clé de capacités d'un rôle est VERSIONNÉE

Le § 4 annonçait `<rôle>_support`. Un `client/hello` réel porte
**`player@v1_support`**, c'est-à-dire le nom du rôle *avec sa version*. Lire
`player_support` rend `None` sur un lecteur courant — donc aucun codec, aucune
fréquence, aucune profondeur.

L'implémentation de référence, côté serveur, lit les deux et qualifie la forme
non versionnée de « legacy ». `messages::ClientHello::support_du_lecteur` fait
de même : clé versionnée d'abord, repli non versionné ensuite.

### 11.2 Les types binaires de fragmentation ont changé

Le § 3 donnait `1` pour la fragmentation et `2` pour l'appairage. L'état
courant est : **`2` = fragment suivi d'autres, `3` = dernier fragment** (le bit
0 porte le drapeau « dernier »). Le type `1` n'est plus attribué à la
fragmentation.

S2-a ne fragmente pas — c'est le sujet de S2-c — et refuse explicitement ce qui
dépasse une trame plutôt que de tronquer. Le point est noté ici pour que S2-c
reparte du fil et non de la section 3.

### 11.3 Charge utile maximale : 65519 et non 65518

`65535 − 16` (le tag AEAD). L'octet de type est **compris** dans cette charge,
il ne s'en retranche pas une seconde fois.

### 11.4 Il existe un mode de transition NON CHIFFRÉ, et c'est aujourd'hui le
seul que parlent les lecteurs publiés

C'est le constat le plus lourd de conséquence.

- La version publiée d'`aiosendspin` (**6.0.5**, celle dont dépend le lecteur
  de référence `sendspin` 7.5.0) **ne contient aucun module `noise/`**. Le
  chiffrement n'existe que dans le dépôt git, pas dans une version publiée.
- Mis face à notre serveur, ce lecteur envoie donc un **`client/hello` en
  clair** comme tout premier message, sans `client/init` ni poignée de main.
- Le serveur de référence (git) aiguille sur le premier message reçu :
  `client/init` → poignée de main Noise ; `client/hello` → connexion **non
  chiffrée acceptée en « mode transition »**, derrière un drapeau
  `allow_unencrypted`, avec la trace « Accepting unencrypted legacy connection ».

Autrement dit : **le Sendspin chiffré est spécifié et implémenté en git, mais
aucun lecteur publié ne le parle encore.** Tune, qui n'implémente que la
branche chiffrée, est conforme à la spécification et **ne peut aujourd'hui
parler à aucun lecteur installé**.

Faut-il implémenter le mode de transition ? C'est une décision de produit —
accepter du non chiffré sur le réseau local pour parler à l'existant, ou
attendre que l'écosystème publie le chiffrement — et elle n'est pas prise ici.

### 11.5 Le prologue Noise est fait des OCTETS EXACTS des deux messages en clair

`prologue = <texte client/init reçu> || <texte server/init envoyé>`, tels qu'ils
ont circulé. Re-sérialiser l'un des deux, même à JSON équivalent, change le
prologue.

Mesuré : l'écart est fatal **dès le message Noise 1** — le prologue entre dans
le hachage `h` avant que la charge utile du premier message ne soit chiffrée,
et le répondeur échoue en la déchiffrant. Il n'atteint jamais le message 2.

### 11.6 La PSK Sentinelle, telle qu'elle est calculée

`SHA-256("sendspin-sentinel-psk-v1")`, et son identifiant
`base64url(SHA-256("sendspin-psk-id-v1" || psk))`. Vérifié : notre dérivation et
celle de l'implémentation de référence donnent le même `psk_id`, et une poignée
de main aboutit dans les deux suites.

Elle est **publique** : elle chiffre, elle n'authentifie personne.

### 11.7 L'implémentation de référence ne sait pas décoder Opus

Alors que la spécification écrit « Servers MUST support all audio codecs », le
SDK de référence refuse qu'un lecteur annonce `opus` : « only PCM and FLAC are
supported ». L'obligation porte sur le serveur, pas sur le lecteur — mais cela
dit qu'en pratique FLAC et PCM suffisent aujourd'hui, ce qui allège S2-c :
l'encodeur Opus n'est pas sur le chemin critique.
