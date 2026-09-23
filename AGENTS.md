# Agents Tune

Le workflow commun est décrit dans `docs/RELEASE-WORKFLOW.md`. La doctrine
canonique est
[`tune-gouvernance/regles/RELEASE.md`](https://github.com/renesenses/tune-gouvernance/blob/main/regles/RELEASE.md)
et le runbook opératoire est `docs/RELEASE-OPERATIONS.md`. Ils s'appliquent
aux agents OpenAI/Codex, Claude et aux humains.

## Base des PR dans ce dépôt

Ces consignes concernent **tune-server-rust** : un correctif vise le lot
`batch/*` assigné, ou la RC assignée si aucun lot ne porte le sujet. Un lot
peut passer par `integration/vX.Y.Z` avant la RC ; seule une branche `rc/*`
peut ensuite viser `main`, conformément au `release-gate` serveur.

Le client web a un circuit distinct : PR vers `main` par défaut, ou vers le
lot/la RC explicitement assigné. Voir la
[table de routage par dépôt](docs/RELEASE-WORKFLOW.md#base-des-pr-par-dépôt).
Ne pas appliquer au web l'exclusivité `rc/* -> main` du serveur et ne pas
changer la cible d'un lot déjà engagé à l'occasion de cet alignement.

## Avant toute modification

1. actualiser les refs et travailler dans un worktree isolé depuis le SHA de
   base du lot ou de la RC assigné ;
2. consulter les issues, commentaires, PR ouvertes et fichiers concernés ;
   vérifier par contenu si le correctif existe déjà et éviter les chevauchements ;
3. lister les labels globaux `verrou:issue-*`, puis acquérir le verrou avec
   `gh label create verrou:issue-N`, **sans `--force`** ; un verrou **détaché**
   se reprend aux conditions de « Verrou détaché » ci-dessous ;
4. ajouter `en-cours` et le verrou à l'issue ; publier l'identité complète
   (personne / fournisseur / run unique), le périmètre, les fichiers prévus,
   la branche, le SHA de base et le worktree Shrek.

Pour une session JP : `JP Robbe / OpenAI Codex / jp-robbe-<date>-<run-unique>`.
« OpenAI/Codex » seul n'identifie pas une session.

```sh
gh api 'repos/renesenses/tune-server-rust/labels?per_page=100' --paginate --jq '.[] | select(.name | startswith("verrou:issue-")) | {name,description}'
gh label create "verrou:issue-N" --repo renesenses/tune-server-rust --color 5319E7 --description "personne / fournisseur / run-unique"
gh issue edit N --repo renesenses/tune-server-rust --add-label "verrou:issue-N" --add-label en-cours
```

Ne jamais utiliser `--force`. Si la création échoue, vérifier l'existence exacte
du label avant toute autre conclusion. Un verrou **posé sur son issue** est
intouchable, quel que soit son âge : ne pas le reprendre sans transfert
explicite. Conserver les verrous pendant la revue ; leur libération suit les
règles du dépôt. La réservation n'est acquise qu'après une création réussie.

### Verrou détaché : conditions de reprise (décision du 23/09/2026)

Un label `verrou:issue-N` **détaché** — existant dans le dépôt mais posé sur
aucune issue — ne réserve plus rien indéfiniment. Il se reprend si, et
seulement si, les **trois** conditions sont réunies :

- **(a)** le label n'est pas posé sur son issue `N` ;
- **(b)** aucune PR ouverte ne cite l'issue `N` ;
- **(c)** plus de **24 h** se sont écoulées depuis la dernière activité **du
  verrou lui-même** — création du label, pose ou retrait du label sur l'issue,
  commentaire de réservation ou de transfert. Un commentaire de tri ou de
  livraison sans rapport avec la réservation ne rajeunit pas un verrou.

```sh
# (a) le label est-il posé sur son issue ?
gh issue view N --repo renesenses/tune-server-rust --json labels \
  --jq '[.labels[].name] | index("verrou:issue-N")'
# (b) une PR ouverte cite-t-elle l'issue ?
gh pr list --repo renesenses/tune-server-rust --state open --limit 500 \
  --json number,title,body,headRefName --jq '.[] | select((.title + .body + .headRefName) | test("(^|[^0-9])N([^0-9]|$)")) | .number'
# (c) dernière activité du verrou : créations et évènements de label
gh api 'repos/renesenses/tune-server-rust/labels/verrou:issue-N' --jq .created_at
gh api 'repos/renesenses/tune-server-rust/issues/N/timeline?per_page=100' --paginate \
  --jq '.[] | select((.event == "labeled" or .event == "unlabeled") and .label.name == "verrou:issue-N") | {created_at, event}'
```

La reprise se **dit** : commentaire de réservation sur l'issue portant la
personne, la session (personne / fournisseur / run unique) et le périmètre,
en indiquant que le verrou détaché est repris au titre de cette règle. Sans ce
commentaire, la reprise n'a pas eu lieu.

Ménage : un verrou détaché remplissant (a), (b) et (c) **dont l'issue est
fermée** ne protège plus rien et peut être supprimé
(`gh label delete verrou:issue-N`). Un verrou détaché dont l'issue est
**ouverte** n'est pas supprimé à la volée : il est repris selon la règle
ci-dessus, ou purgé sur arbitrage humain.

**Pourquoi cette règle.** La rédaction antérieure — « un label existant réserve
l'issue même s'il n'y est pas attaché », sans limite de temps — condamnait à ne
plus jamais être traitées les issues dont le verrou avait survécu à sa session.
Le constat à l'origine de la décision : **207 verrous détachés sur 248**, et
plusieurs sessions arrêtées le 23/09 devant ce texte, à juste titre. Un verrou
protège un travail en cours, pas la mémoire d'un travail fini.

Règles non négociables :

- une PR unitaire cible le lot courant `batch/<thème>`, ou la RC `rc/vX.Y.Z`
  quand aucun lot ne porte le sujet ; elle ne contient pas de bump de version ;
- `ci:full` est obligatoire pour les changements CI, release ou transversaux ;
- dans **ce dépôt serveur**, seule une RC peut cibler `main` ;
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
