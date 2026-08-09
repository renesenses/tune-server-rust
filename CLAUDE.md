# CLAUDE.md — tune-server-rust

Doctrine de branches, de merge et de release. **À lire avant de créer une
branche, d'ouvrir une PR ou de taguer.** Le déroulé complet d'une release est
dans `docs/RELEASE-OPERATIONS.md` ; ce fichier-ci dit *où* va le code et
*pourquoi*.

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

4. **Ne courez pas après une branche qui bouge.** Si des PR continuent
   d'atterrir, taguez ce qui est testé et laissez le reste au train suivant.
   Poussez branche et tag **dans la même commande** pour réduire la fenêtre.

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

- **`Ferme #123` ne ferme rien.** GitHub ne reconnaît que les mots-clés
  **anglais** : `Closes` / `Fixes` / `Resolves`. Le corps de PR peut rester en
  français, la ligne de liaison doit être en anglais.

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
