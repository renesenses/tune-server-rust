# Peut-on déplacer tout Spotify dans un plugin Tune ?

L'implémentation serveur est désormais dans le **plugin natif**
`plugins/tune-spotify` : OAuth, Connect, catalogue natif, appairage, workers,
page/JavaScript d'appairage, callback OAuth, Ogg et PlayPlay/FLAC, avec leurs
tests. Le cœur ne dépend plus de librespot
ou du fournisseur PlayPlay ; ses dépendances protobuf propres à Spotify ont
été déplacées également (protobuf reste transitif pour Chromecast).
Les identifiants de source et espaces
de credentials restent inchangés. Migration locale, pas une release.

## Utilisation

- Feature serveur `spotify` : plugin OAuth/Connect, incluse dans `default` pour
  préserver la disponibilité historique. `--no-default-features` permet de
  construire un serveur sans aucune implémentation Spotify.
  Les composeurs et recettes de packaging utilisant `--no-default-features`
  doivent nommer `spotify` (ou `spotify-native`) s'ils veulent le conserver ;
  aucun workflow de release n'est modifié dans cette unité locale.
- Feature `spotify-native` : ajoute `tune-spotify/native`. Il faut toujours
  `TUNE_SPOTIFY_NATIVE=1` pour choisir cette implémentation au runtime.
- `plugin_spotify_enabled=false` : source et routes absentes au prochain
  démarrage ; credentials conservés. Pas de nouveau processus ni appairage.
- `streaming_spotify_enabled` et les deux clés `auth_tokens_spotify` /
  `auth_tokens_spotify_native` continuent à être restaurés sans migration DB.
- Le plugin est chargé par défaut pour préserver les installations existantes,
  mais cela n'active ni authentification ni lecture automatique.

Les routes Connect vivent dans le plugin (`/api/v1/ext/spotify/…`). L'hôte
conserve l'alias `/api/v1/spotify-connect/…`, avec les mêmes protections
d'authentification. Cet alias disparaît quand le plugin est désactivé.
La page d'appairage, son script et le callback OAuth sont également dans le
plugin. Leurs anciennes URL `/api/v1/streaming/spotify/{native-pairing,
native-pairing.js,callback}` sont des alias exacts ; ils ne capturent pas les
autres routes Streaming génériques. Le callback utilise un hook générique
d'invalidation du cache utilisateur après authentification, comme avant.
Les anciennes options de configuration et le champ `spotify_redirect_uri`
restent des frontières de compatibilité côté serveur, pas une seconde
implémentation. Les composants du client web ne sont pas déplacés ici.

## Ce que le code permet réellement

Le modèle natif existe : `TunePlugin`, `PluginContext`, `PluginBuilder`, et
Bandcamp séparé dans `plugins/tune-bandcamp`. Il peut être compilé dans Tune
ou composé hors arbre depuis un binaire dépendant de `tune-server`.
Ce n'est pas un `.so`/`.dylib` installé à chaud : un changement du plugin
natif exige aujourd'hui de reconstruire le binaire qui le compose.

Un runtime WASM existe bien dans `tune-plugin-runtime-wasm`. Les anciens textes
du guide qui disent « pas de runtime WASM » sont dépassés sur ce point.
Mais les imports effectivement enregistrés sont log, queue get/add, now playing,
play, pause et événements. Pas de sessions réseau Spotify, discovery LAN,
processus enfant ou producteur PCM. Le RFC n'est pas une preuve que toutes ses
capacités projetées sont disponibles. WASM ne convient donc pas à l'ensemble
actuel sans refaire une grande part de l'intégration côté hôte.

## Frontière recommandée

| Dans le plugin `tune-spotify` | Dans le cœur Tune |
| --- | --- |
| Session/appairage et persistence dans l'espace de credentials Spotify | Stockage et interfaces génériques d'authentification |
| Catalogue, favoris, playlists, métadonnées et leurs limites | `StreamingService`, registre et routes Streaming génériques |
| Workers librespot, IPC, annulation propre et producteur PCM | Contrat générique de résolution audio et gestion des sessions/zones |
| Sélection Ogg/FLAC, PlayPlay, fournisseur de clé et CDN | Transport HTTP Tune et formats audio observés |
| Ancien OAuth/Connect si ces modes sont conservés | Configuration/lifecycle des plugins, sans types Spotify |

Le code et ses tests sont déplacés : ce plugin n'est pas une façade laissant
l'implémentation dans `tune-core`.

## Points d'extension ajoutés

1. **Enregistrement différé.** SDK 1.1 : `register_streaming_service` collecte
   le service pendant `setup`. Un conflit avec le cœur, un autre plugin ou au
   sein du plugin refuse toutes ses inscriptions, y compris ses routes, puis
   appelle `teardown`. Le registre refuse aussi atomiquement un conflit tardif.
2. **Restauration ciblée.** L'hôte restaure uniquement les nouveaux services
   avant de démarrer les événements et d'accepter des requêtes ; il ne relance
   pas la restauration des services déjà présents.
3. **Audio privé.** `StreamingService::private_audio` /
   `resolve_private_audio` remplacent les downcasts Spotify. Le contrat impose
   un nouveau décodage pour chercher une position, la confirmation avant
   publication, le maintien de la pause et le refus de DSP non appliqué.
4. **Workers avant bootstrap.** `PluginWorker` et `RunOptions::workers`
   permettent aux plugins compilés ou composés hors arbre d'apporter leurs
   entrées privées. Un flag inconnu ou possédé deux fois refuse le démarrage.
   Les workers Spotify conservent leur IPC et leur supervision de processus.
5. **Arrêt.** Le plugin arrête Connect et le service natif sans effacer les
   credentials. L'autodémarrage Connect préexistant suit `system.started`.

## Reste hors de cette extraction

Le fournisseur PlayPlay privé reste externe ; ni APK ni clé ni table dans le
plugin distribué. Sélecteur de qualité, extension UI dédiée et chargement à
chaud ne sont pas ajoutés. La provenance FLAC a été raccordée ensuite : le
plugin publie le codec confirmé sur sa session, le cœur le porte dans
`NowPlaying.format`, et le chemin du signal distingue source FLAC/OGG et
transport WAV sans promettre un navigateur bit-perfect.
Bandcamp n'est pas migré vers le nouveau hook. La CI multiplateforme reste
nécessaire avant intégration ; une compilation Shrek n'est pas une release.

Commandes ciblées :

```sh
cargo test -p tune-spotify --features native --lib --locked
cargo test -p tune-core --lib --no-default-features --features oaat,plugin-http plugin_ --locked
cargo test -p tune-core --lib --no-default-features --features oaat,plugin-http private_audio --locked
cargo test -p tune-server --no-default-features --features oaat,spotify-native --test spotify_plugin --locked
```

Contre-épreuve locale : les tests restent inchangés tandis que les gardes de
collision, de présence du worker et de résolution/seek privé sont retirées.
Compilation réussie, puis huit échecs attendus (six SDK/worker, deux audio).
La restauration se fait par copie, SHA-256 identiques, avant la relance verte.
Cette preuve porte sur les interfaces d'extraction, pas sur une lecture DAC.

Les alias HTTP ont leur contre-épreuve séparée : enlever leurs trois montages
(sans modifier les tests) donne deux échecs attendus, page d'appairage et
callback OAuth en 404 au lieu de 200. Les routes Streaming génériques ne sont
pas capturées par ces alias. La restauration est ensuite validée par la même
suite d'intégration.

## Validation ciblée du 2026-09-15

- Linux/Shrek : 26 tests SDK/worker, 2 audio privé, 29 contrats plugins,
  6 intégrations Spotify, 105 tests du plugin, 31 HTTP Streaming, puis
  13 contrats web avec Spotify et 12 sans Spotify. Tous réussis ; les dix
  rouges des deux contre-épreuves compilables précèdent leur restauration.
- macOS ARM : binaire de validation sans sortie audio locale, SHA-256
  `1f55e55ea8dcbca46b12cad2c4d82c0ad8a983f53d66cbd0905f8caea59919cf`.
  Worker réel avec le compte déjà appairé et le fournisseur privé : FLAC
  44,1 kHz/16 bits stéréo complet (41 239 968 octets PCM), MD5 conforme au
  STREAMINFO indépendant ; seek à 101 123 ms identique au suffixe exact du
  décodage complet ; arrêt/récolte du worker ; refus d'un format non supporté
  sans PCM ni repli silencieux ; Ogg toujours fonctionnel.
- PCM haché puis jeté, aucun périphérique audio ouvert, aucun volume modifié.
  Instance existante non remplacée ; zones et files inchangées avant/après.

Ces preuves ne valident ni un renderer/DAC, ni Windows, ni la CI multiplateforme,
ni une distribution signée. Le fournisseur PlayPlay externe reste nécessaire.
