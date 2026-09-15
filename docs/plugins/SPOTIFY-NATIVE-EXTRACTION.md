# Peut-on déplacer tout Spotify dans un plugin Tune ?

Oui, dans un **plugin natif**, avec quelques extensions génériques du cœur.
Cette note décrit une migration proposée, pas une migration déjà effectuée.
Constat sur la branche `feat/spotify-native`, base locale `a4f0f71b`.

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

Il faut déplacer le code et ses tests, pas créer une façade qui laisse toute
l'implémentation dans `tune-core`.

## Points d'extension manquants

1. **Enregistrement des services.** `PluginRegistrations` sait transporter
   sorties, fournisseurs de sorties, routes et zones, pas de `StreamingService`.
   Ajouter une inscription atomique avec refus des noms déjà possédés. Préserver
   l'ordre restauration des credentials / démarrage et le mode dormant.
   Bandcamp est encore enregistré directement dans `tune-server/src/state.rs`.
2. **Résolution audio privée et seek.** `orchestrator/resolve_stream.rs` et
   `orchestrator/transport.rs` font aujourd'hui des downcasts vers
   `SpotifyNativeService`. Les remplacer par un contrat générique optionnel
   de producteur audio/seek avec annulation et format observé.
3. **Entrée worker avant bootstrap.** `tune-server/src/bootstrap.rs` appelle
   directement `spotify_native::run_worker_if_requested`. Prévoir un dispatch
   de worker côté composition avant configuration/DB/logs, ou un sidecar natif
   versionné et supervisé. Ne pas lancer un serveur complet dans chaque worker.
4. **Anciens chemins spécifiques.** Sortir `configured_spotify`,
   `SpotifyConnectManager` et les routes Connect de l'état central, avec routes
   de compatibilité si des clients les utilisent. Conserver les identifiants
   de source `spotify` et les espaces de credentials séparés.
5. **Qualité et interface.** Décrire les capacités d'authentification/qualité
   via le contrat, pas par des tests du nom Spotify. Garder les composants de
   catalogue/file génériques ; les écrans Connect spécifiques peuvent ensuite
   suivre le mécanisme d'extension UI choisi par Tune.

## Migration par étapes, sans casser l'appairage

1. Valider le chemin PlayPlay derrière un fournisseur indépendant — cette unité.
2. Ajouter et tester les contrats génériques, en gardant les autres sources
   inchangées : conflit d'inscription, plugin désactivé, échec setup, arrêt,
   restauration, seek en pause, credential namespace.
3. Extraire les modules dans une caisse native optionnelle ; retirer les
   dépendances librespot/PlayPlay du graphe `tune-core` sans Spotify.
4. Câbler catalogue et UI/plugins, puis recette silencieuse de bout en bout et
   CI d'intégration multiplateforme. Le packaging du fournisseur PlayPlay reste
   un sujet distinct de cette extraction Rust.

La demande actuelle pose la question de cette migration ; elle n'autorise pas
implicitement une release, le déplacement d'autres services ni la distribution
de l'APK analysé. Aucune de ces opérations n'est effectuée ici.
