# Reprise de lecture — serveur #7280

Bertrand Clech / OpenAI Codex / bertrand-20261028-reprise-r8m2.
Base assignée : `batch/bugs-rc9-20261025` @ `3036a3ec782e858272feaea18fc9523b5fbad566`.
Contrat écrit avant le code : `contrat-reprise-de-lecture.md`.

## Changement

Un contexte n'entre dans la reprise qu'au démarrage d'un titre. L'instantané conserve les identifiants de bibliothèque ou de service, les métadonnées du conteneur, l'ordre réellement en file et le dernier titre démarré. Les ajouts contextualisés n'héritent pas de l'album de session. La reprise reconstruit atomiquement la file et rejoue ce titre depuis zéro. Le cas d'un titre seul ajouté depuis un conteneur retrouve sa suite dans l'album/playlist. Les artistes, labels et radios ne sont pas proposés par ce widget.

Les podcasts conservent leur dernier épisode et sa position ; les nouveaux clients transmettent leur identifiant stable et leur image. Le recul est un réglage serveur exposé au web : entier 0–300, défaut 10 secondes, conformément à la décision du 28/10. Migration SQLite 135 / PostgreSQL 100 (99 réservée à la sentinelle des anciennes bascules) pour la table des reprises.

## Preuves déjà exécutées sur Shrek

- `cargo test -p tune-core --lib reprise_fabien_7280 -- --nocapture` : 2 réussites. Ajout absent avant démarrage puis présent, rang relatif au conteneur, reconstruction de la suite, ordre mélangé, titre seul.
- `cargo test -p tune-server --no-default-features --features local-audio --lib reprise_fabien_7280 -- --nocapture` : 5 réussites. Contexte puis GET, podcast et position, vraie route de reprise avec contrôle de sortie factice (recul 17 s), réglage GET/PATCH et rejets, titre ajouté depuis un album et suite entière.
- `cargo test -p tune-core --test integration_contracts aucune_fuite_de_temporaires` : 3 réussites.

Un premier témoin core a trouvé dans le correctif un rang global 3 au lieu du rang de playlist 1. La sélection exclut désormais les lignes héritant de la session quand le titre joué porte son propre contexte ; le test est resté inchangé et passe.

## Validation encore en cours à l'ouverture du brouillon

La contre-épreuve compilable et Clippy attendent le garde d'admission de Shrek (mémoire / plafond de builds). Un sixième témoin de la route musicale vérifie aussi le rejeu depuis zéro après un seek récent et le dernier titre sélectionné. Aucun résultat de contre-épreuve non exécuté n'est annoncé. La CI complète est demandée avec `ci:full`, conformément à la règle de 10 h 45 ; aucune suite complète locale.

Aucune sortie réelle, aucun son, aucun accès au .18 ni service réel. Les témoins utilisent SQLite et des sorties/provisions d'URL factices ; PostgreSQL et la batterie complète restent à la CI. Pas de fusion, version, tag ou publication.

### Rejeu explicite après un seek
Le sixième témoin a révélé la coalescence d’une reprise avec la lecture courante : 42000 ms conservées au lieu de zéro. La route passe désormais par `replay_context_from_queue`, entrée explicite distincte des doubles appuis, qui efface le seek précédent et la garde réseau de cette zone. Les gestes ordinaires gardent leurs protections. Rejeu ciblé et contre-épreuve en cours sur Shrek ; aucun vert supplémentaire annoncé avant leur résultat.
