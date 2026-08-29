# CLAUDE.md — tune-server-rust

Doctrine de branches, de merge et de release. **À lire avant de créer une
branche, d'ouvrir une PR ou de taguer.** Le déroulé complet d'une release est
dans `docs/RELEASE-OPERATIONS.md` ; ce fichier-ci dit *où* va le code et
*pourquoi*.

---

## 0. Check-list de démarrage — AVANT d'ouvrir un éditeur

Ces quatre commandes prennent trente secondes. Les sauter a déjà coûté des
heures de travail jeté.

```bash
# 1. Des refs à jour, sinon tout ce qui suit ment.
git fetch origin --tags --force

# 2. LE CORRECTIF EXISTE-T-IL DÉJÀ ? Sur les DEUX lignes, et par CONTENU.
git grep "<un symbole/marqueur du correctif>" origin/release/v0.9
git grep "<un symbole/marqueur du correctif>" origin/main
gh pr list --state merged --search "<mots-clés du bug>" --limit 10

# 3. Une branche depuis la ligne qui LIVRE, dans un worktree isolé.
git worktree add ../wt-<sujet> -b fix/<sujet> origin/release/v0.9

# 4. Les versions se lisent sur origin/, JAMAIS dans la copie de travail.
git show origin/main:package.json | grep '"version"'
```

**Le point 2 est celui qu'on saute, et c'est le plus cher.** Vécu le
2026-08-09 : une heure passée à réimplémenter la pochette par piste (#1284)
alors que le correctif était déjà mergé sur `release/v0.9` sous un autre
numéro de fil (#1312 → PR #1344). Une seule commande `git grep` l'aurait
évité.

Cherchez par **contenu**, pas par numéro : le même bug arrive souvent sous
deux numéros — celui de l'issue GitHub et celui du fil forum.

Et si vous touchez au schéma : une colonne s'ajoute **aux quatre endroits**

| # | endroit | pour qui |
|---|---|---|
| 1 | `CORE_SCHEMA` (`db/sqlite.rs`) | base SQLite neuve |
| 2 | migration SQLite (`db/migrations.rs`) | base SQLite existante |
| 3 | `PG_FULL_SCHEMA` (`db/pg_migrate.rs`) | base PG neuve, et bascule SQLite→PG |
| 4 | migration PG (`migrations/postgres/NNN_….sql` + `PG_MIGRATIONS`) | **base PG existante** |

**Longtemps on en comptait trois** — et c'est le quatrième qui manquait. Les
colonnes du chantier CUE n'existaient que dans le `CREATE TABLE` de
`pg_migrate.rs` : aucune base PostgreSQL existante ne les a jamais reçues, et
personne ne s'en est aperçu pendant des mois parce qu'aucune requête ne les
nommait encore (#2111). Le test `pg_schema_parity` refuse désormais tout écart
entre 3 et 4.

Si la copie SQLite→PG lit la colonne, l'ajouter **aussi** au bloc de rattrapage
en fin de `PG_FULL_SCHEMA` : sans quoi l'`INSERT` de la table entière échoue et
la bibliothèque arrive vide.

Et **jamais par un `ALTER TABLE ADD COLUMN` dans `up:`** — sur une base neuve la
colonne existe déjà, l'ALTER échoue en « duplicate column name » et fait
planter tout `run_migrations` au premier démarrage. Utilisez
`add_column_if_missing`.

---

## 1. Topologie — la seule chose à retenir

```
release/v0.9   ← les fixes NAISSENT ici. Les tags SORTENT d'ici. C'est ce qui est livré.
     │
     └──merge──▶  main   ← ne reçoit QUE des merges. Ne livre RIEN.
```

- **Toute branche de fix part de `origin/release/v0.9`** et cible
  `release/v0.9` en PR.
- **`main` ne reçoit jamais de PR de fix directement.** Elle se met à jour par
  merge après chaque release.
- Un fix mergé sur `main` **ne sera jamais livré** tant qu'il n'est pas sur
  `release/v0.9`. C'est l'erreur la plus coûteuse de ce dépôt : en juillet 2026,
  cinq fixes ont raté quatre trains de suite pour cette raison.

### ⚠️ `tune-web-client` est l'INVERSE

Le client web **ship depuis `main`**. Un fix web mergé sur son `release/v0.9`
ne sera jamais livré. Ne pas transposer la règle du serveur.

---

## 2. Ne corrigez pas le même bug deux fois

Avant d'écrire un fix, vérifiez qu'il n'existe pas déjà **sur l'autre ligne**,
y compris sous un autre numéro de PR :

```bash
git fetch origin
git grep "<un marqueur du fix>" origin/release/v0.9
git grep "<un marqueur du fix>" origin/main
```

Deux implémentations du même bug sur les deux lignes produisent un **conflit
sémantique** au merge suivant — pas un conflit de texte : deux stratégies
différentes, toutes deux plausibles, non fusionnables.

**Si ça arrive quand même**, la règle de résolution est :

> On garde l'implémentation **livrée** — celle qui est dans le tag, donc celle
> qui est passée par le gate de tests et qui tourne chez les utilisateurs.
> L'autre est écartée. Si elle est jugée meilleure sur le fond, elle doit être
> **re-proposée sur `release/v0.9`, avec un test** — jamais réintroduite sur
> `main`, qui ne livre pas.

Documentez la résolution dans le message de merge : le prochain agent tombera
sur la même question.

---

## 3. L'audit de portage ment (et c'est normal)

`scripts/preflight-port-audit.sh` signale les PR `base=main` non portées. Il
cherche **le numéro de PR** dans les messages de commit de `release/v0.9`. Il
produit donc des **faux positifs** dans deux cas parfaitement légitimes :

1. le contenu est arrivé par un **merge de rattrapage** (le merge ne porte pas
   le numéro de la PR) ;
2. le fix a été **ré-implémenté sous un autre numéro** sur la ligne de release.

**Ne jamais conclure sur le numéro. Vérifier par CONTENU :**

```bash
git grep "<marqueur du fix>" origin/release/v0.9   # présent ⇒ faux positif
```

Un audit rouge n'autorise pas à taguer *tant qu'on n'a pas tranché chaque
ligne*. Mais une ligne vérifiée présente par contenu **n'est pas** un blocage.

---

## 4. Gate de tests et tag

1. **Gate obligatoire avant tout tag**, sur le commit exact qui sera tagué :
   ```bash
   cargo test --workspace --no-default-features --features tune-server/oaat
   ```
   Zéro échec, non négociable. Capturez la sortie dans un fichier
   (`> gate.log 2>&1`, dans cet ordre) et vérifiez le **code de sortie** —
   un `| tail` masque l'échec.

2. **Le tag sort du commit testé.** Le commit de bump doit avoir pour parent
   le commit gaté.

3. **Si `release/v0.9` a avancé pendant les tests** → `git rebase` du bump sur
   le nouveau tip **et re-gate**. Vérifiez d'abord la nature du delta : s'il
   ne touche aucun `.rs` ni `Cargo.toml/lock`, le résultat se reporte.

4. **Ne courez pas après une branche qui bouge — gelez-la.** Vécu le
   2026-08-11 sur la v0.9.68 : **quatre gates** ont été nécessaires, chaque
   passage de ~20 min étant doublé par une PR touchant du `.rs`, donc sans
   report possible du résultat.

   ⚠️ « Taguer ce qui est testé » ne veut PAS dire taguer un commit dépassé :
   le tag serait **hors de la ligne**, et c'est exactement ce qui a produit les
   incidents v0.9.63 et v0.9.66. Le commit tagué doit descendre du tip **au
   moment du push**.

   ```bash
   # Geler pendant la fenêtre de release (les admins gardent le droit de pousser)
   gh api -X PUT repos/renesenses/tune-server-rust/branches/release/v0.9/protection \
     -f 'lock_branch=true' -F 'enforce_admins=false' \
     -F 'required_status_checks=null' -F 'required_pull_request_reviews=null' \
     -F 'restrictions=null'

   # Dégeler DÈS que les assets sont publiés — sinon toute l'équipe est bloquée
   gh api -X DELETE repos/renesenses/tune-server-rust/branches/release/v0.9/protection
   ```

   Au push, le serveur confirme l'exemption : *« Bypassed rule violations —
   Cannot change this locked branch »*. Et enchaînez **gate → re-fetch →
   vérification du tip → bump → tag → push dans une seule commande**, pour
   qu'il ne reste aucune fenêtre entre la fin des tests et le push.

   ⚠️ Le dégel **supprime toute la protection**. Si la branche en portait une
   en régime normal (checks requis, force-push interdit), la reposer après.

---

## 5. Merger vers `main`

Après la release, on merge **le TAG**, pas le tip de branche :

```bash
git merge --no-ff v0.9.XX -m "Merge release/v0.9 (v0.9.XX) into main"
```

`main` reçoit ainsi **exactement ce qui a été livré**, sans les commits arrivés
après le tag. Et on **relance la suite de tests sur la fusion avant de
pousser** : un merge sans conflit peut casser la compilation.

---

## 6. Pièges qui coûtent cher

- **Une ligne de fermeture peut agir avec retard.** GitHub n'auto-ferme pas au
  merge sur `release/v0.9`, mais relit les mots-clés lorsque cet historique
  rejoint la branche par défaut. La synchronisation de v0.9.125 vers `main` a
  ainsi fermé #1897 à partir d'un ancien message qui disait littéralement
  `This does not close #1897` : la négation est ignorée par GitHub (#2785).

  Le test qui tranche : l'événement `closed` porte un `commit_id` quand c'est
  GitHub qui ferme, et **aucun** quand c'est un humain.

  ```bash
  gh api repos/renesenses/tune-server-rust/issues/<n>/events \
    --jq '.[]|select(.event=="closed")|"\(.actor.login) \(.commit_id//"aucun")"'
  ```

  Pour déclarer une vraie fermeture, utilisez `Closes`/`Fixes`/`Resolves` et
  attendez la preuve dans la release publiée. Pour dire explicitement qu'un
  commit **ne ferme pas** une issue, n'écrivez jamais ces mots-clés après une
  négation : écrivez seulement `Refs #N`. Le garde-fou lit aussi les messages
  des commits propres à la PR, car filtrer le seul corps ne protège pas le
  prochain passage tag → `main`.

  ⚠️ Une PR de `tune-web-client` qui vise une issue de `tune-server-rust` doit
  écrire la forme complète — `Closes renesenses/tune-server-rust#2036` — sinon
  GitHub résout le numéro dans le dépôt **de la PR** et ne désigne rien.

- **Lisez les versions sur `origin/`, jamais dans la copie de travail.** Les
  dépôts locaux traînent souvent sur une vieille branche d'une autre session :
  `git show origin/main:package.json`, pas `cat package.json`.

- **Travaillez en worktree isolé** depuis `origin/release/v0.9`. Le dépôt
  principal appartient probablement à une autre session ; ne commitez jamais
  ses modifications non commitées.

- **Bumpez le web AVANT de taguer le serveur.** `release.yml` a une garde
  *« Verify web-client version matches release tag »* qui fait échouer toute la
  release sinon.

- **Ne bumpez jamais `tune-server-linux`** (serveur Python, mort et gelé).

- **Le bump et le tag sont la décision de Bertrand.** Ne les lancez pas de
  votre propre initiative.

---

## 6 bis. Une release publiée se solde — clôture et réponse aux testeurs

**Le stock d'issues doit refléter l'état du code.** Il ne le fait pas au bon
moment : `Closes` peut n'agir qu'au passage différé vers `main`, et un correctif
fusionné n'est pas un correctif livré. Sans cette étape, on accumule des issues
ouvertes dont le défaut n'existe plus — mesuré le 28/08/2026 : **23 issues P2
traitées dans la journée, 0 fermée, 373 issues ouvertes**. Le compteur ne
mesurait plus le travail, il mesurait l'oubli.

**Pendant le cycle de développement**, on ne touche pas au suivi : ni triage,
ni fermeture, ni commentaire spontané. **Dès qu'une release est PUBLIÉE**, on
solde ce qu'elle livre.

### Ce qu'il faut faire, dans cet ordre

1. **Établir ce que la release livre réellement**, issue par issue.
2. **Fermer à la main** — `gh issue close`, puis `gh issue view <n> --json state`
   pour vérifier au lieu de supposer.
3. **Commenter** en disant **à partir de quelle version** c'est corrigé. Un
   testeur en version antérieure verra encore le défaut : sans le numéro de
   version, la réponse est inutilisable.

### La preuve exigée avant de fermer

**Le contenu de l'artefact publié**, jamais autre chose. Ni « PR mergée », ni
`git tag --contains`, ni `--is-ancestor` — une fusion peut écraser du contenu,
et un correctif peut manquer son tag de quelques minutes. C'est arrivé **trois
fois en août** : #2317 raté de 17 min, #2404 de 30 min, le kiosque d'un jour.

Un `grep` dans le binaire ou dans le `web/` du tarball, **avec un cas témoin
sur une version antérieure où le marqueur doit être ABSENT** (`rc=1`). Sans
témoin, une absence de résultat ne prouve rien : elle peut venir d'un marqueur
mal choisi. Choisir un littéral qui survit à la compilation — message de
journal, code d'erreur, champ sérialisé — **jamais un nom de fonction**, que
la minification et le linker effacent.

### Ce qu'on ne ferme pas

- les issues portant **`keep-open`** ;
- celles dont le correctif est fusionné mais **absent de l'artefact publié** —
  elles attendent la release suivante ;
- **jamais de fermeture en masse au jugé.** Une issue fermée à tort ne se
  rouvre pratiquement jamais, et le testeur qui l'avait ouverte le vit comme
  un abandon.


---

## 7. Plusieurs agents travaillent ce dépôt — le verrou est obligatoire

Vous n'êtes pas seul. Le 2026-08-20, **quatre PR ont touché `verdict_purge`
dans la même journée** (#1983, #2016, #2017, #2038) : trois ont conflité, et
l'une a failli livrer un `sudo` dans un conteneur `debian:bookworm` nu, ce qui
aurait cassé la construction de Tune OS **à tous les coups**. Aucune n'était
mauvaise ; elles s'ignoraient.

### DEUX verrous, dans cet ordre : l'issue d'abord, la zone ensuite

**Jamais deux agents sur la même issue.** Le verrou de zone ne suffit pas : il
protège le fichier, pas le sujet. Deux agents peuvent parfaitement prendre la
même issue si elle touche deux zones, ou si l'un travaille sans rien réserver.

Le verrou d'issue emploie le même mécanisme atomique, et il se prend **en
premier** — c'est le moins cher et le plus précis :

```bash
set -o pipefail
gh label create "verrou:issue-<n>" --repo renesenses/tune-server-rust \
  --color 5319E7 --description "<qui> — <ISO8601>"          # échec ⇒ prise
gh issue edit <n> --repo renesenses/tune-server-rust --add-label "en-cours"
```

L'étiquette `verrou:issue-<n>` est le verrou (invisible, atomique) ;
`en-cours` posée SUR l'issue est là pour qu'un humain voie d'un coup d'œil ce
qui est pris. Les deux se rendent ensemble, à la fusion ou à l'abandon :

```bash
gh label delete "verrou:issue-<n>" --repo renesenses/tune-server-rust --yes
gh issue edit <n> --repo renesenses/tune-server-rust --remove-label "en-cours"
```

**Échec du `label create` = l'issue est prise. On en choisit une autre.** Ne
jamais se contenter de regarder si `en-cours` est posée : cette lecture-là
n'est pas atomique, et deux agents qui regardent en même temps concluent tous
les deux que c'est libre.

### Prendre le verrou de la zone AVANT d'écrire

`gh label create` **échoue si l'étiquette existe déjà** : c'est un test-and-set
atomique, donc un vrai mutex.

```bash
set -o pipefail   # sinon un `| tail` masque l'échec et le verrou ne sert à rien
gh label create "verrou:<zone>" --repo renesenses/tune-server-rust \
  --color B60205 --description "<qui> — issue #<n> — <ISO8601>"
```

**Échec = quelqu'un tient la zone.** On prend une issue ailleurs. On ne « passe
pas quand même ».

Rendre le verrou dès que la PR est fusionnée ou abandonnée :

```bash
gh label delete "verrou:<zone>" --repo renesenses/tune-server-rust --yes
```

Zones : `scan` · `queue` · `qobuz` · `tidal` · `dsd` · `dlna` · `chromecast` ·
`bluos` · `airplay` · `enrichissement` · `acoustique` · `licence` · `ci` ·
`web`. Une issue qui en traverse deux prend **les deux**, dans l'ordre
alphabétique — sinon deux agents se bloquent mutuellement.

Un verrou dont l'horodatage dépasse quelques heures sans PR ouverte peut être
cassé, mais **en le disant** dans l'issue concernée, jamais en silence.

### Le circuit des tickets est automatisé — n'y touchez pas à la main

Quatre agents s'en chargent (`~/.claude/agents/`) :

| agent | rôle | déclencheur |
|---|---|---|
| `tune-tri` | lit les tickets, ouvre les issues | forum modifié (toutes les 10 min) |
| `tune-chef` | classe, attribue, tient le verrou | sur appel |
| `tune-retour` | établit ce qui est livré, ferme les issues | nouveau tag |
| `tune-parole` | écrit aux testeurs, sous l'identité de Bertrand | nouveau tag |

Conséquences pour vous :

- **N'écrivez pas aux testeurs de votre propre initiative.** Deux réponses en
  doublon ont dû être supprimées le 2026-08-16, dont une qui prescrivait ce que
  l'utilisateur venait de dire avoir essayé.
- **Ne fermez pas une issue sur un merge — fermez sur un TAG.** Vérifiez :
  `git merge-base --is-ancestor <sha> <tag>`. Un correctif fusionné après le
  tag n'est pas livré, et l'annoncer est pire que se taire.
- **Vérifiez le périmètre avant de fermer.** Le défaut dominant est le
  demi-correctif déclaré complet : #1893 fermée avec trois routes sur six,
  #1969 fermée sans son cache. Relisez ce que l'issue demandait, point par
  point. Certains messages de commit disent eux-mêmes « ne prétend PAS
  résoudre #N » — lisez-les.

### Vérifier, pas supposer

- `cargo clippy --all-targets`, **jamais** `cargo check` seul : il ne compile
  pas les tests et laisse filer les erreurs jusqu'à la CI (vécu sur #2016).
- **Ne jamais fusionner sur `mergeable_state: unknown`** — GitHub n'a pas fini
  de calculer et le merge peut consommer un sha périmé. Attendre `clean`,
  `behind` ou `unstable`, et épingler `-f sha=$head`.
- **L'auto-merge n'actualise PAS la branche.** Avec `strict: true`, une PR
  entièrement verte mais `BEHIND` reste armée indéfiniment sans que rien ne se
  passe. Il faut pousser `update-branch`.
- **Pendant un gel : désarmez les auto-merge.** Un auto-merge armé fusionne dès
  que sa CI passe, y compris au milieu d'une fenêtre de release — c'est ce qui
  a fait tomber la première tentative de la v0.9.92.
