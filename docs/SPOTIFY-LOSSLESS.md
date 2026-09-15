# Spotify natif : chemin PlayPlay / FLAC expérimental

Unité locale #4166, continuation de `feat/spotify-native`. Pas une release.
Ce chemin est distinct de l'ancien client OAuth et du lecteur Ogg de librespot.

## Activation explicite

Compiler avec `spotify-native`, conserver `TUNE_SPOTIFY_NATIVE=1`, puis :

```sh
export TUNE_SPOTIFY_LOSSLESS=16
export TUNE_SPOTIFY_PLAYPLAY_HELPER=/chemin/absolu/vers/le/helper-local
```

`TUNE_SPOTIFY_LOSSLESS` absent / vide / `0` / `off` conserve la voie Ogg.
`16` demande le format Spotify 16 (FLAC 16 bits) ; `24` demande le format 22.
Toute autre valeur est refusée. Une qualité demandée mais indisponible ou une
licence refusée n'est **jamais** remplacée silencieusement par de l'Ogg.
Les réglages s'appliquent au processus serveur, pas encore à chaque zone.

Le helper est actuellement nécessaire et **n'est pas livré dans le dépôt**.
Le prototype local utilise Python/Unicorn et l'ELF de l'APK déjà vérifié ; il
ne constitue pas une réimplémentation Rust autonome. Aucun téléchargement
automatique, aucune APK, table propriétaire, clé de contenu ou donnée de compte
n'est embarqué dans Tune. Ce backend local reste lié au hash exact du binaire
analysé. Sa disponibilité et sa distribution ne sont pas une propriété du SDK.

L'appairage Tune existant suffit : pas de nouveau Client ID ou mot de passe.
Il ne garantit pas que Spotify autorisera le FLAC pour tout compte/morceau.

## Chemin effectif

1. Le worker audio restaure la session existante, puis lit `AUDIO_FILES`.
2. Il sélectionne strictement le format demandé et son identifiant de fichier.
3. Il demande une licence PlayPlay v5 avec la session authentifiée. Les refus
   HTTP restent des erreurs ; aucun contournement ou réessai de 403/429.
4. Le fournisseur local transforme les 16 octets obfusqués en clé AES-128.
5. Le résolveur `/storage-resolve/v2/files/audio/interactive/{format}/{id}`
   fournit les URL ; les credentials ne sont jamais transmis aux CDN.
6. Requêtes Range HTTPS : sonde de 4 Kio, puis fenêtres de 128 Kio, redirections
   interdites, taille/cohérence vérifiées, délai réseau borné. Alternatives
   uniquement pour 404/410 sur les URL déjà renvoyées par Spotify.
7. AES-128-CTR seekable ; `fLaC` et STREAMINFO vérifiés **avant** d'annoncer du
   PCM. Symphonia décode en entiers sans normalisation, rééchantillonnage ou
   réduction des 24 bits. Un seek redémarre au bon échantillon.
8. PCM vers le canal Tune borné, puis WAV HTTP avec fréquence/profondeur/canaux
   effectivement décodés. Aucun fichier audio ou clé n'est mis en cache.

Les erreurs de trame, ruptures de chronologie, longueur incomplète et MD5
négatif du décodeur produisent une erreur, pas une fin normale. Le MD5 global
n'est contrôlable que pour une lecture complète depuis le début ; après seek,
les contrôles de trames, chronologie et longueur restent actifs.

## Contrat du fournisseur local

Un exécutable configuré par chemin absolu, sans shell ni arguments secrets.
Son environnement est vidé ; un script doit nommer un interpréteur absolu
dans son shebang, pas dépendre du PATH ou des credentials de l'hôte.
Entrée/sortie : longueur u32 big-endian + JSON, limite des réponses 4 Kio,
délai 20 secondes par échange. stderr n'est jamais recopié dans les journaux.

```json
{"operation":"info","protocol":1}
{"protocol":1,"playplay_version":5,"token":"<16 octets hex>"}
{"operation":"derive","protocol":1,"file_id":"<20 octets hex>","obfuscated_key":"<16 octets hex>"}
{"protocol":1,"key":"<16 octets hex>"}
```

Les valeurs ci-dessus sont des marqueurs, pas des clés. Le fournisseur ne
reçoit ni identifiants Spotify, ni URL, ni musique. Il est arrêté/récolté avant
le décodage. Sur Unix, le worker a un groupe de processus propre : son arrêt
ou sa sortie fatale annule aussi un fournisseur encore en calcul. Ce contrat est refusé
sur Windows tant que la supervision des descendants n'y est pas implémentée.
Le processus local est de confiance : l'IPC borné ne constitue pas une sandbox
du code natif du fournisseur.

## Limites conservées

- Opt-in expérimental ; stabilité du protocole non officiel non garantie.
- 24 bits pris en charge et éprouvés avec des données synthétiques ; leur
  disponibilité réelle chez Spotify reste à vérifier sur un titre proposé.
- Une seule zone active, pas de DSP de zone, comme le prototype précédent.
- Fréquences acceptées : 8–192 kHz ; mono/stéréo ; PCM 16/24 bits.
- L'affichage du chemin du signal reste conservateur : un WAV Spotify n'est
  pas encore marqué lossless. La provenance FLAC du worker doit être transportée
  jusqu'à ce calcul avant de lever ce garde-fou ; ne pas déduire le codec de
  la préférence demandée. Aucune promesse de bit-perfect jusqu'au DAC/browser.
- Pas de sélecteur de qualité dans l'interface ni d'installation automatique
  du fournisseur. L'implémentation serveur a depuis été extraite dans
  `plugins/tune-spotify` ; voir la note d'extraction.

## Validation

Suite ciblée :

```sh
cargo test -p tune-spotify --lib --locked --features native spotify_native
```

Les tests synthétiques couvrent protobuf malformé, qualité stricte, IPC,
fournisseur incompatible, arrêt du descendant, vrais échanges HTTP locaux,
redirections/refus/troncatures, recalage CTR non aligné, FLAC 16/24 bits
44,1/48/96 kHz octet pour octet, seek exact et longueur tronquée.
Ils n'ouvrent jamais de périphérique audio et n'utilisent aucune vraie clé.

Pour les vérifications de compte : binaire worker seul, stdin anonyme,
stdout PCM consommé puis jeté, base en lecture seule ; aucune zone/file ne
doit être changée. **Ne jamais baisser/muter le volume système du Mac.**
Une cross-compilation n'est pas une validation runtime macOS.

### Résultats du 2026-09-15

- Shrek : 72 tests natifs Spotify, 33 tests `tune-streaming-http`, puis
  28 tests du connecteur OAuth avec `spotify-native` désactivé : réussis.
  17 nouveaux témoins, dont arrêt du descendant lors d'un abandon **et**
  d'une sortie fatale. Les avertissements préexistants restent visibles.
- Contre-épreuve compilée : remplacer le recalage CTR par un retour à zéro
  et retirer la droitisation des échantillons fait échouer quatre témoins
  (CTR, HTTP + seek, PCM exact, seek exact). Tests inchangés ; sources
  restaurées par copie, hashes identiques, suite finale verte. Une première
  variante bloquée par un import inutilisé n'est pas comptée comme preuve.
- macOS ARM : vrai `tune-server` cross-compilé, exécuté en mode worker seul.
  Appairage existant ; licence et clé calculées de nouveau par le fournisseur.
  FLAC 44,1 kHz / 16 bits / stéréo : **41 239 968 octets PCM**, fin normale,
  MD5 égal au STREAMINFO déchiffré indépendamment lors de l'analyse initiale.
- Seek à 101 123 ms : **23 401 872 octets**, hash identique à la même portion
  du décodage complet. Arrêt forcé du worker en 5 ms lors de la recette finale.
  Une demande de format non pris en charge est refusée sans PCM ; le chemin
  Ogg existant fournit toujours sa fin de piste et son EOF normal.
- Le helper isolé retrouve la clé de référence hors réseau avec un environnement
  vide. Aucun secret d'environnement du serveur n'est transmis au fournisseur.
- Aucun périphérique audio ouvert, aucune musique jouée, volume système du
  Mac inchangé. Instance existante non remplacée/redémarrée ; états/volumes et
  contenus des deux tables de file comparés avant/après et inchangés.
  Seul `current_track.metadata_age_ms`, âge dérivé croissant sur chaque GET,
  est exclu de l'égalité des réponses des zones ; leurs autres champs restent
  comparés. Le premier contrôle brut de cet âge était donc impropre.

Binaire de validation (pas un artefact signé/notarisé de distribution), SHA-256 :
`958f6eab4348183796edc3738b70d035ca756463034b41af5ef5eb037dacff61`.
Les dix fichiers d'entrée de compilation ont les mêmes SHA-256 localement
et sur Shrek. Journaux, recettes, snapshots et binaire conservés dans le
répertoire privé de validation, pas dans Git. Pas de CI GitHub ou de recette
UI/renderer de bout en bout pour le nouveau chemin dans cette unité.

Voir [l'analyse d'extraction en plugin](plugins/SPOTIFY-NATIVE-EXTRACTION.md).
