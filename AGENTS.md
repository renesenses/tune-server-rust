# Agents Tune

Le workflow commun est décrit dans `docs/RELEASE-WORKFLOW.md`. Il s'applique
aux agents OpenAI/Codex, Claude et aux humains.

Avant toute modification :

1. mettre les refs à jour et travailler dans un worktree isolé ;
2. vérifier que le correctif n'existe pas déjà par contenu ;
3. acquérir le verrou avec `gh label create verrou:issue-N` ;
4. ajouter `en-cours` à l'issue et annoncer fournisseur, run, branche et SHA de
   base.

```sh
gh label create "verrou:issue-N" --repo renesenses/tune-server-rust --color 5319E7 --description "fournisseur/run"
gh issue edit N --repo renesenses/tune-server-rust --add-label "verrou:issue-N" --add-label en-cours
```

Si la création échoue, vérifier l'existence du label exact. Label présent :
issue prise. Label absent : panne d'infrastructure, donc arrêt sans écrire.

Règles non négociables :

- une PR unitaire cible `rc/vX.Y.Z` et ne contient pas de bump de version ;
- `ci:full` est obligatoire pour les changements CI, release ou transversaux ;
- seule une RC peut cibler `main` ;
- un agent de correctif ne merge pas, ne tague pas et ne publie pas ;
- un échec, un check absent ou une situation inconnue bloque le travail ;
- les instructions locales peuvent durcir ces règles, jamais les assouplir.

La PR indique l'issue, la RC, l'identité de l'agent, les preuves exécutées et
ce qui n'est pas traité.
