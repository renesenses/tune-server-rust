# #3462 — diagnostics CLI, découverte UDP et LMS sans platine

JP Robbe / OpenAI Codex / jp-robbe-20260916-002200-3462

Base : `8946d3806682e1875c596d329e609f032995ebb5`,
lot `batch/p2-idle-poll-diagnostics-20260915` (inclut #4200).
Développement et validations sur Shrek, worktree et target
`jp-robbe-20260916-002200-3462`, 6 jobs.

## Comportements exercés

La cible existante `sante_slimproto_i3462` contient désormais cinq parcours
HTTP/sockets : la garde TCP précédente et quatre nouveaux tests. Elle reste
un processus de test séparé, puisque les états d'écoute sont globaux.

- Deux vrais bind refusés sur ports TCP/UDP déjà occupés, avec protocole,
  port, cause et erreur système conservés.
- Santé HTTP 200 et base `ok`, composants CLI/UDP à `false`, diagnostic
  réseau et rapport de bogue porteurs des causes.
- Arrêt explicite, reprise sur ports éphémères réels, double armement UDP
  ignoré, commande CLI `version ?`, réponse UDP `eNAME`, puis arrêt sans
  composant de panne résiduel.
- LMS bouchonné sur TCP loopback : zéro platine, réponse invalide, une platine.
  Les vrais routeurs HTTP et le code de recensement/enregistrement sont appelés.
  Aucune variable d'environnement n'est modifiée par ces tests.

Commande :

```sh
cargo test -p tune-server --test sante_slimproto_i3462   --no-default-features --features oaat
```

Premier passage : 5 réussis, 0 échec, 0 ignoré ; compilation 5 min 28 s,
exécution 0,98 s. Après contre-épreuve et restauration par copie :
5 réussis, 0 échec, exécution 1,09 s.

## Contre-épreuve sémantique

Tests conservés, trois comportements antérieurs rétablis temporairement :
pas d'état retenu après bind refusé ; HTTP 200 sur découverte vide ;
réponse de comptage invalide convertie en zéro.

Même commande, compilation réussie, sortie 101 : **3 échecs / 2 réussites**.

| Témoin rouge | Message ou valeurs observées |
| --- | --- |
| `cli_udp_conflit_reprise_arret_sont_visibles_sans_degrader_la_base` | `une tentative réelle doit retenir son état: Elapsed(())` |
| `lms_joignable_sans_platine_explique_le_refus_de_decouverte` | `une découverte LMS sans platine ne doit plus rendre un succès vide` ; 200 au lieu de 409 |
| `une_reponse_lms_invalide_ne_devient_pas_zero_platine` | `lms_sans_platine` au lieu de `lms_recensement_impossible` |

Les fichiers corrigés ont été restaurés par `cp` depuis leurs sauvegardes.
SHA-256 du test inchangé avant/après :
`f9d7afe16728581c72731132879e8cba8a8fab22abd36a73c480d507a6f4819a`.

## Compatibilité et limites

Lecture du client web à `f9cf9dcfea1bcd3ad3009a426ecda4e5287286c9` :
`api.ts` transforme le champ `error` d'une réponse HTTP non réussie en
`Error.message` ; les actions de découverte de `SettingsView.svelte`
et `v2/SettingsV2.svelte` affichent ce message dans une notification.
Le POST de découverte vide renvoie donc 409 avec `code=lms_sans_platine`
et `error`. Le GET d'état et le recensement périodique gardent leur succès
sur une liste vide. Les nouveaux détails JSON de l'état restent disponibles
aux clients ; aucune nouvelle vue client n'est livrée ici.

Ce constat client est une lecture de code, pas une recette visuelle.
Aucun LMS/HQPlayer matériel du testeur, aucun journal privé et aucun écran
de son installation n'ont été utilisés. Une liste vide ne prouve pas quel
équipement ou pont manque. Les volets démarrage/avatar restent hors périmètre.

Le patch de #4211 à `50af74e5a1110756d2bf2f90496e72db05e2f52b` passe `git apply --check` sur ce worktree sans être appliqué :
ses modifications de `diagnostics.rs` sont dans les métriques du poller,
celles-ci dans les sections réseau/rapport. Aucun workflow, migration ou
numéro de version modifié.


## Suites connexes

- `cargo test -p tune-core --lib --no-default-features --features oaat slimproto::` :
  34 réussis, 0 échec.
- `cargo test -p tune-server --lib --no-default-features --features oaat routes::squeezebox::` :
  10 réussis, 0 échec.

Soit 49 tests ciblés réussis, dont 6 nouveaux (4 parcours HTTP/sockets et
2 gardes de durée de vie de l'état). Ce périmètre ne vaut pas recette matérielle.

- `cargo clippy -p tune-server --test sante_slimproto_i3462 --no-default-features --features oaat -- -D clippy::correctness` :
  succès, 2 min 52 s ; avertissements non bloquants présents.
- `cargo fmt --all -- --check` et `git diff --check` : succès.
