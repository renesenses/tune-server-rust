# Agents Tune

Le workflow commun est décrit dans `docs/RELEASE-WORKFLOW.md`. La doctrine
canonique est
[`tune-gouvernance/regles/RELEASE.md`](https://github.com/renesenses/tune-gouvernance/blob/main/regles/RELEASE.md)
et le runbook opératoire est `docs/RELEASE-OPERATIONS.md`. Ils s'appliquent
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

- une PR unitaire cible le lot courant `batch/<thème>`, ou la RC `rc/vX.Y.Z`
  quand aucun lot ne porte le sujet ; elle ne contient pas de bump de version ;
- `ci:full` est obligatoire pour les changements CI, release ou transversaux ;
- seule une RC peut cibler `main` ;
- un agent de correctif ne merge pas, ne tague pas et ne publie pas ;
- sans instruction humaine explicite portant sur l'étape précise, aucun agent
  ne modifie ruleset, environnement, secret ou variable d'armement ;
- un dry-run vert ne donne jamais l'autorisation de franchir le STOP humain
  suivant ;
- un échec, un check absent ou une situation inconnue bloque le travail ;
- les instructions locales peuvent durcir ces règles, jamais les assouplir.

La PR indique l'issue, la RC, l'identité de l'agent, les preuves exécutées et
ce qui n'est pas traité.

## ⛔ Ce dépôt est PUBLIC — aucune revue de sécurité en issue

`renesenses/tune-server-rust` est **public** et compte des forks. Toute analyse
qui dit **où et comment contourner un contrôle** — licence, droits Premium,
relais cloud, authentification — ne va **jamais** dans une issue ni dans un
commentaire d'issue : c'est un mode d'emploi publié.

Elle va dans un **avis de sécurité privé** (brouillon, invisible du public) :

```bash
gh api repos/renesenses/tune-server-rust --jq .visibility      # à vérifier AVANT d'écrire
gh api -X POST repos/renesenses/tune-server-rust/security-advisories --input avis.json
# avis.json : {summary, description, severity, vulnerabilities:[…]} — `vulnerabilities` est OBLIGATOIRE
```

Si une telle issue existe déjà : l'**archiver**, en recopier le contenu dans un
avis privé, puis la **supprimer** — la fermer ne suffit pas, une issue close
reste lisible :

```bash
ID=$(gh api repos/renesenses/tune-server-rust/issues/<n> --jq .node_id)
gh api graphql -f query='mutation($id:ID!){deleteIssue(input:{issueId:$id}){repository{name}}}' -f id="$ID"
```

Et le dire sans fard : la suppression ferme la porte, elle ne rembobine pas —
qui a synchronisé un fork pendant l'exposition a le texte.

Vécu le 17/09/2026 : une revue de licence publiée en issue, avec chemins et
numéros de ligne, restée publique deux heures. Un commentaire de réponse avait
en plus annoncé qu'un contrôle de production était inactif.

## Ce qu'est une preuve

« N tests réussis » mesure un **périmètre**, pas une propriété. Un vert ne
prouve rien tant qu'on n'a pas montré que le témoin sait rougir.

Avant d'annoncer qu'un correctif est couvert : retirer le **correctif** — pas
le test — et relancer. Le test doit rougir, et son message doit nommer le
défaut. Restaurer ensuite par copie (`cp` d'une sauvegarde prise avant), jamais
par `git checkout --` qui emporte aussi le reste du travail, puis relancer une
dernière fois pour revenir au vert.

Trois rouges ne valent pas contre-épreuve :

- un rouge de **compilation** : le sabotage a cassé le code, pas la propriété.
  Refaire le sabotage de sorte que tout compile encore ;
- un rouge d'un **autre** test que celui présenté comme témoin ;
- un rouge obtenu en modifiant le test plutôt que le code gardé.

Si le rouge ne vient pas, c'est le témoin qui est en défaut, pas le correctif
qui est prouvé. Deux causes fréquentes, toutes deux constatées ici :

- une garde de texte satisfaite par sa propre cible : `contains("fn ma_fn(")`
  est vrai grâce à la ligne de **définition**. Compter les occurrences hors
  définition ;
- un fichier de `tests/` sans entrée `[[test]]` dans le `Cargo.toml` :
  `autotests = false` fait qu'il n'est **jamais compilé**. Un banc de
  1 027 lignes gardant 30 routes est resté ainsi treize essais durant, tous
  annoncés verts parce qu'aucun n'existait.

La PR écrit le résultat de la contre-épreuve : la commande passée, le test qui
a rougi, la ligne de son message. Une PR qui ne le dit pas déclare, par
omission, que la contre-épreuve n'a pas été faite.
