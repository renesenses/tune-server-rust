# #3810 — refuser une sélection de candidats en panne

JP Robbe / OpenAI Codex / jp-robbe-20260918-suite2-3810

Base : `73707a08c1658289913058a9843250622be08521`.
Branche : `fix/jp-robbe-20260918-3810-enrichment-query`.
Lot : `batch/jp-p2-enrichment-20260918`.
Worktree Shrek : `/srv/builds/worktrees/jp-3810-20260918-suite2` ;
target isolé sous la même clé.

## Défaut et antériorité

La PR #3930 (298098e0) a déjà ajouté les traces de départ, de candidats et de
refus d'enrichissement. Elles sont dans la base et ne sont pas refaites.

Le défaut restant est précis : `query_many` échoue pendant la sélection des
candidats, son erreur devient `Vec::new()`, puis la tâche écrit
`status=done, total=0, errors=0` et émet `library.enrich.completed`.
Une panne SQL est donc présentée comme une sélection vide réussie.

Le signalement de Tades ne prouve pas cette cause. Le correctif vise le
comportement reproductible du serveur, sans conclure sur son installation.

## Contrat corrigé

La sélection locale s'exécute dans `spawn_blocking`, attendu par la route,
**après** la résolution de portée et le gate existants, **avant** la trace
de départ, l'état `running`, l'annonce de tâche et le HTTP 202.

En cas d'erreur SQL ou d'échec de la tâche bloquante :

- HTTP 500 avec `{"error":"enrichment_candidates_unavailable"}` ;
- journal `enrich_all_selection_failed` avec la cause technique ;
- aucun identifiant de nouvelle tâche, aucune nouvelle tâche annoncée ;
- état d'enrichissement précédent conservé ;
- aucun événement de progression ou de fin réussie.

Le gate et son contrat ne sont pas modifiés : la sélection est placée
**après** lui, donc une requête admise par ce gate conserve ses effets même
si la sélection échoue ensuite. Le témoin vérifie cet ordre et le refus
existant quand le quota est déjà épuisé.

Une sélection vide valide conserve HTTP 202, un identifiant de tâche, puis
`done` et zéro erreur. Aucun appel MusicBrainz n'est nécessaire dans ce cas.

### Coût et limite

L'acquittement HTTP 202 attend désormais la sélection SQL locale. Sur une
grande bibliothèque, il peut donc arriver plus tard ; aucune mesure de cette
latence sur les 262 858 pistes de Tades n'a été faite. `spawn_blocking` évite
de bloquer un thread asynchrone pendant la requête, mais ne réduit ni le coût
SQL ni la taille de la sélection. Seule la sélection locale est attendue ;
les appels MusicBrainz restent dans la tâche détachée. Celle-ci peut être
ordonnancée avant l'envoi effectif du 202 : aucune garantie d'ordre temporel
entre la réponse HTTP et son premier appel réseau n'est ajoutée.

Les pannes ultérieures de MusicBrainz, les écritures de progression, les
relances simultanées, l'affichage des erreurs côté client et la politique de
quota restent hors du correctif.

## Témoins et résultats

`cargo test -p tune-server --lib enrich_selection_tests_3810 -- --test-threads=2`

Deux témoins passent par le routeur HTTP réel et une base SQLite mémoire
migrée :

1. `tracks` renommée dans la seule base de test : HTTP 500 générique,
   aucun `task_id`, ancien statut conservé via GET, registre de tâches vide
   et aucun événement. Le gate garde son comportement.
2. Bibliothèque vide valide : HTTP 202 puis événement de fin et GET
   `status=done, total=0, errors=0`, identifiant cohérent.

Aucun compte, licence distante, serveur MusicBrainz ou autre service tiers.
La panne est injectée sur SQLite seulement ; aucune panne PostgreSQL, mesure
volumétrique ni borne de temps sur une grosse sélection n'est éprouvée ici.

Premier passage : **2/2 verts** en 0,41 s, après 5 min 10 de compilation.
Après restauration :
- `cargo test -p tune-server --lib routes::library::enrich:: -- --test-threads=2` :
  **11/11 verts** (2 nouveaux, 9 préexistants), 0,67 s ;
- `cargo test -p tune-server --test enrichissement_audible_3810` :
  **1/1 vert**, 0,25 s ; la garde d'instrumentation et de refus existante reste exécutée ;
- `cargo fmt --all -- --check` et `git diff --check` : verts.

`cargo clippy -p tune-server --lib -- -D clippy::correctness` :
vert en 2 min 40. Les 348 avertissements de la bibliothèque serveur restent
hors des lignes modifiées ; les positions signalées dans `enrich.rs` ont
été comparées au fichier de base. Aucun avertissement n'est introduit sur
les ajouts de production. Les nouveaux tests ont été compilés et exécutés ;
ce passage Clippy cible la bibliothèque de production.

## Contre-épreuve

Tests inchangés, production seule modifiée ; rétablir la conversion de la
panne en liste vide doit faire rougir le témoin HTTP, sans casser la
compilation. Restauration par `cp` et contrôle SHA-256 avant le vert final.

La production a été changée uniquement dans le bras d'échec de sélection :
retourner `Vec::new()` au lieu du refus HTTP. Les tests restent inchangés.

Compilation réussie (36,02 s), puis **1 rouge / 1 vert**, Cargo 101.
`une_selection_en_echec_refuse_le_lancement_sans_fausse_reussite` échoue
à `enrich_selection_tests_3810.rs:51` :

> une panne de sélection ne doit pas être acceptée comme une passe vide réussie

HTTP reçu **202**, attendu **500**. La bibliothèque vide valide reste verte.
La restauration `cp` et le contrôle SHA-256 des deux fichiers réussissent.
Retour au vert final : **11/11 dans le module**, puis **1/1 dans le témoin HTTP existant**.

Refs #3810 ; aucun bump, merge, tag ou déploiement. Le verrou reste pendant
la revue. Aucune clôture de l'incident terrain.
