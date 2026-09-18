# #4377 — états numériques du protocole HQPlayer

JP Robbe / OpenAI Codex / jp-robbe-20260917-p1suite3-4377.

Base : 73707a08c1658289913058a9843250622be08521.
Lot : batch/jp-p1-audio-transport-20260917.
Branche : fix/jp-robbe-20260917-4377-hqplayer-state.
Worktree : /srv/builds/worktrees/jp-4377-20260917-suite3.
Target dédié : jp-4377-20260917-suite3.

## Défaut démontré

Le parseur cherchait les mots playing/paused/stopped dans toute la réponse.
Une réponse officielle Status.state=2 était donc traitée comme Stopped ;
inversement, un titre égal à playing pouvait masquer un état arrêté ou inconnu.

Source primaire actuelle :
[HQPlayer Control 6.0.1, archive officielle Signalyst](https://www.signalyst.eu/bins/hqp-control-601-src.zip).
ControlInterface.hpp définit 0=STOPPED, 1=PAUSED, 2=PLAYING, 3=STOPREQ ;
ControlInterface.cpp lit l'attribut state de Status comme entier.
Archive conservée sur Shrek :
/srv/builds/jp-evidence/jp-4377-20260917-suite3/hqp-control-601-src.zip
SHA-256 d51bbc5652abf34713003367de0e0edf0036fadccc4660cd0eeb2ea55eb4a1aa.

Source primaire historique : [Jussi Laako (Signalyst), 21 novembre 2017](https://community.roonlabs.com/t/roon-hqplayer-stops-when-new-track-sample-rate-changes/33502/32)
confirme ces quatre valeurs et décrit STOPREQ comme transitionnel, avant STOPPED.
Ses messages 34 et 36 dans le même fil retracent ce contrat aux versions antérieures.
Pas de SDK5 téléchargé ni de capture brute du HQPlayer Embedded5 de ce testeur :
la conformité protocolaire est établie, la causalité de son incident reste à confirmer.

## Changement

Le parseur quick-xml (dépendance déjà présente) vise Status.state, accepte
les états numériques et conserve les formes textuelles, y compris un enfant
State direct. Le champ explicite a priorité ; aucune recherche de mots dans
les titres, attributs voisins ou autres métadonnées.

STOPREQ devient Transitioning : le poller doit attendre Stopped pour constater
une fin de piste. Aucun changement du poller, de ses seuils, des commandes,
du volume ou des unités. Un état inconnu conserve Stopped et le diagnostic
existant, émis une seule fois par sortie.

Le témoin historique hqplayer_avance_album_4023 utilisait state=2 comme
valeur inconnue. Cette fixture est remplacée par 99, hors enum officiel ;
ses assertions sur le repli et le diagnostic unique sont conservées.

## Témoins et limites

Nouveau module privé enregistré depuis hqplayer.rs, sans modification Cargo.
Un serveur TCP local répond aux vraies requêtes get_status de HqplayerOutput :

- états numériques 0/1/2/3 et champs de position/durée ;
- titre playing face à state=0, enfant State contradictoire, attribut data-state ;
- state inconnu ou absent malgré des métadonnées playing : diagnostic conservé ;
- compatibilité textuelle, guillemets simples, espaces et déclaration XML ;
- témoin direct du parseur : un State après la fermeture de Status est ignoré.

Le faux serveur vérifie le décodage et le contrat OutputStatus. Il ne joue aucun
audio et ne prouve ni l'avance réelle d'album ni une correction terrain.

## Validation

Exécution sur Shrek avec un target isolé, CARGO_BUILD_JOBS=1 et deux threads
de tests, compte tenu de la charge partagée. Environnement chargé explicitement
depuis /srv/cache/tune/env.sh avec la clé ci-dessus.

Commandes ciblées :

```sh
cargo test -p tune-core --lib outputs::hqplayer:: --no-default-features --features oaat -- --test-threads=2
cargo test -p tune-core --test hqplayer_avance_album_4023 --no-default-features --features oaat -- --test-threads=2
cargo fmt --all -- --check
cargo clippy -p tune-core --lib --no-default-features --features oaat -- -D clippy::correctness
git diff --check
```

Résultats acquis :

- module HQPlayer : 18 tests réussis, dont les cinq nouveaux ;
- contre-épreuve compilable : restauration du seul ancien parseur, tests
  inchangés, 13 réussites et 5 échecs attendus ; notamment state=2 retourne
  Stopped alors que le témoin attend Playing ;
- restauration par copie du fichier vert, contrôle SHA-256 des trois sources :
  identiques ; nouveau passage du module : 18 réussites ;
- cible explicitement enregistrée hqplayer_avance_album_4023 : 1 réussite ;
- cargo fmt : réussi ;
- Clippy lib avec -D clippy::correctness : réussi (434 avertissements
  non bloquants dans le graphe, sans politique générale de refus des warnings) ;
- contrôle final git diff --check : réussi.

Le premier passage de compilation a rencontré E0308 : quick-xml 0.41 renvoie
BytesText pour read_text. L'appel a été corrigé avec decode puis unescape,
et l'API d'attribut non obsolète a été utilisée, avant le premier passage
vert et les sauvegardes d'empreintes. Cet échec de compilation n'est pas la
contre-épreuve comportementale ci-dessus.

Preuves conservées sur Shrek dans
/srv/builds/jp-evidence/jp-4377-20260917-suite3/ :
green-1.log (échec de compilation initial), green-2.log (premier vert),
counterproof.log et counterproof.exit (101 attendu), fixed-sources.sha256,
hqplayer.rs.fixed, green-restored.log, legacy-4023.log, fmt.log, clippy.log.

Empreintes des sources testées et restaurées :

| Fichier | SHA-256 |
| --- | --- |
| hqplayer.rs | ed2c9b1f73c93e86619e306ec37298bf3a68c4224e7e09da8602829e88617f02 |
| hqplayer_state_tests_4377.rs | 0657879d5c491fde52ecd75a0f6a42183a60c1ca73909f947aa8dd1402dbdc1e |
| hqplayer_avance_album_4023.rs | 3195c0d9d7604a66fd23416e88fbdb02421eb971c3656287b4984984de3bf90c |

Ces résultats portent sur le décodage réel d'une réponse protocolaire via TCP
et les diagnostics existants. HQPlayer Embedded5, l'appareil du testeur,
la lecture audio et l'avancement d'un album réel n'ont pas été exécutés.
Les contrôles CI du lot restent distincts.
