# Workflow de release Tune

`main` est la source de vérité. La branche permanente `release/v0.9` n'est
plus utilisée pour préparer ou taguer les releases.

## 1. Correctifs

```text
fix/* ou feat/* -> PR -> rc/vX.Y.Z
```

La PR exécute le profil rapide : formatage, analyse statique, tests unitaires
et régressions ciblées. Le label `ci:full` force la batterie complète pour un
changement transversal ou risqué. Aucun bump de version n'est fait dans une
PR unitaire.

## 2. Candidat de release

La RC contient tous les correctifs retenus, les versions et les références
immuables des composants. Quand elle est prête :

```text
rc/vX.Y.Z -> PR -> main
```

Cette PR exécute la batterie complète. Le check agrégateur `release-gate`
échoue si la branche source n'est pas une RC ou si un job obligatoire est
rouge, annulé, absent ou ignoré. PostgreSQL reste un check requis séparé.

Toute modification de la RC après le verdict invalide les checks et relance la
batterie sur la nouvelle tête.

## 3. Tags et staging

Après fusion des RC vertes, le contrôleur vérifie les quatre `main` puis crée
les tags web, Universal, OS et enfin serveur. Seul le tag serveur déclenche le
train. Ce train :

1. conserve la GitHub Release serveur en brouillon ;
2. pousse Docker uniquement sous `staging-vX.Y.Z` ;
3. transmet à Tune OS le SHA OS, la version serveur et les deux SHA-256 Linux ;
4. attend les trois builds OS et leurs tests ;
5. conserve leur release en brouillon.

Le tarball serveur attesté est embarqué dans chaque image OS. Le premier
démarrage n'interroge ni `releases/latest`, ni une branche flottante.

## 4. Promotion

`Promote staged release` est le seul workflow qui déplace des canaux stables.
Son dry-run est obligatoire. Après approbation de l'environnement protégé, une
exécution idempotente recopie le digest Docker staged vers `vX.Y.Z` et
`latest`, publie les releases OS et serveur, met Homebrew à jour puis notifie
le site. Android reste inchangé tant qu'il n'est pas ajouté explicitement au
manifeste du train.

Les agents de correctif ne fusionnent pas, ne créent pas de tag et ne publient
aucun canal.

Les actifs sont préparés avant de déplacer un canal public. Une reprise utilise
le même tag et le même numéro ; un incident d'infrastructure ne consomme pas
une nouvelle version.

## Coordination des agents

OpenAI/Codex, Claude et les humains suivent le même circuit. Avant d'écrire,
un agent crée le label atomique `verrou:issue-N`, ajoute `en-cours` à l'issue
et indique dans la PR son fournisseur, son run, l'issue, la branche et le SHA
de base. Si la création échoue, l'agent vérifie le label exact : présent,
l'issue est prise ; absent, l'infrastructure est en erreur et l'agent s'arrête.

Une consigne locale peut renforcer ces règles, jamais autoriser un push direct
sur `main`/`rc/*`, un merge, un tag ou une publication.
