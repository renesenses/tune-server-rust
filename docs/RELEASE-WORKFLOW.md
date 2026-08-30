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

## 3. Tag et publication

Après fusion de la RC verte, le contrôleur de release vérifie que le commit est
dans `main`, crée automatiquement le tag `vX.Y.Z`, puis construit les paquets.
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
