# #4234 — annuler le téléchargement d'une lecture dépassée

JP Robbe / OpenAI Codex / jp-robbe-20260916-4234-download-cancel

Base : `3a2b710a151a257e217143b2900eeb53076ea6ef`.
Développement et validation sur Shrek, target propre à cette unité, six jobs.
Aucun compte de service musical utilisé.

## Propriété corrigée

Le chemin local/OAAT téléchargeait le fichier compressé entier dans une tâche
bloquante avant de vérifier si le canal PCM avait encore un consommateur.
Retirer sa session arrêtait la lecture logique, mais pas ce téléchargement.

Le téléchargement HTTP est désormais asynchrone et lié à l'existence de sa
session. Le registre est observé toutes les 25 ms ; une session déjà retirée
ne déclenche aucune requête. Le retrait pendant l'attente des en-têtes ou du
corps abandonne la future réseau. Cette cadence n'est pas une garantie temps
réel : elle dépend de l'ordonnancement.

Une connexion HTTP de lecture peut retenir un `Arc<StreamSession>` après son
retrait du registre. `Sender::closed()` seul ne suffit donc pas. Deux témoins
retiennent réellement cette référence et vérifient que le canal reste ouvert
pendant que le téléchargement, lui, se termine et ferme sa connexion amont.

`NamedTempFile` possède le fichier téléchargé et le supprime à l'abandon,
sur erreur et après décodage. Le crate était déjà verrouillé pour les tests ;
il devient une dépendance normale, sans changement de version ni de lockfile.
Le refus HTTP conserve la suppression de la session inutilisable (#3287).
Le marqueur d'annulation nomme `stream_id` et `zone_id`.

## Validation de comportement

```sh
export TUNE_TARGET_KEY=jp-robbe-20260916-4234-download-cancel
export CARGO_BUILD_JOBS=6 CMAKE_BUILD_PARALLEL_LEVEL=6 NUM_JOBS=6 MAKEFLAGS=-j6
export CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0
. /srv/cache/tune/env.sh
cargo test -p tune-core --lib --no-default-features --features oaat i4234 -- --nocapture
cargo test -p tune-core --lib --no-default-features --features oaat orchestrator::
cargo test -p tune-core --lib --no-default-features --features oaat http::streamer::tests
```

- Dix nouveaux tests passent après restauration, aucun ignoré.
- 297 tests de l'orchestrateur passent, **dont les dix nouveaux**.
- 43 tests du gestionnaire de flux passent.
- Total de ces deux suites : 340 tests distincts.

Le serveur HTTP du banc écoute uniquement sur une adresse loopback et un port
éphémère. Les cas couvrent : en-têtes muets ; corps partiel réellement écrit
sur disque ; session déjà retirée ; préchargement vivant sans lecteur et
conservation exacte de 250 000 octets ; HTTP 403 ; corps tronqué ; annulation
du producteur réel ; refus HTTP du producteur réel ; décodage HTTP réel avec
vrai EOF ; fichier DASH préexistant conservé, octets inchangés.

## Vérifications complémentaires

`cargo fmt --all -- --check` et `git diff --check` passent.
Clippy réussit avec les avertissements existants du dépôt :

```sh
cargo clippy -p tune-core --lib --tests --no-default-features --features oaat -- -D clippy::correctness
```

## Contre-épreuve

La surveillance de disparition a été remplacée par une future qui ne se
termine jamais. Aucun autre comportement ni test n'a changé. Même commande
`i4234` : **compilation réussie, puis quatre échecs et six succès**.

Témoins rouges :

- `i4234_cancel_before_http_headers_closes_upstream`
- `i4234_cancel_partial_body_with_retained_session_removes_file`
- `i4234_real_transcode_task_exits_when_session_is_removed`
- `i4234_removed_session_never_starts_a_request`

Messages : `removed session must cancel its pending HTTP download`,
`real local/OAAT producer must exit without waiting for obsolete download`,
`an already removed session must finish without contacting upstream`,
suivis de `Elapsed(())`.

SHA-256 de la section des dix tests, identique avant/après sabotage :
`5b16b4f1e56b73e365b7ab73631279563ef0a3aaaee1211c667dc82db4e637c1`.

Restauration du fichier par copie de la sauvegarde, comparaison du contenu et
du SHA des tests, recompilation et retour des dix témoins au vert. Le nettoyage
d'erreur historique est réutilisé avec un chemin optionnel puisque le garde
a déjà supprimé le téléchargement partiel.

## Limites

Ce banc prouve l'annulation et le nettoyage sur Linux, sans DAC ni Qobuz réel.
Il ne mesure pas le gain de débit chez le testeur et n'attribue pas son OOM.
La défaillance ALSA et la transaction de lecture de #4235 restent distinctes.
Les chemins Range et d'acquisition DASH ne reçoivent pas de nouveau mécanisme
d'annulation dans ce correctif ; le fichier DASH déjà présent est éprouvé ici.
Aucun test ni redémarrage du prototype Spotify désactivé.
