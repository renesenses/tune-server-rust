# Workflow de release Tune

`main` est la source de vérité. La branche permanente `release/v0.9` n'est
plus utilisée pour préparer ou taguer les releases.

## 1. Correctifs

```text
fix/* ou feat/* -> PR -> batch/<thème> -> PR -> rc/vX.Y.Z
```

Les correctifs sont groupés par thème dans une branche de lot `batch/*`. Chaque
correctif y entre par sa propre PR ; le lot entre ensuite dans la RC par une PR
unique, qui est la vraie porte. Quand aucun lot ouvert ne porte le sujet, une PR
unitaire peut viser directement la RC.

`scripts/determiner-profil-ci.sh` traite les deux bases à égalité : une PR dirigée
vers `batch/*` comme vers `rc/*` exécute le profil rapide — formatage, analyse
statique, tests unitaires et régressions ciblées. Toute autre base, tout push et
toute entrée inconnue basculent en batterie complète : le routage est fail-closed.
Le label `ci:full` force la batterie complète pour un changement transversal ou
risqué. Aucun bump de version n'est fait dans une PR unitaire.

Avant d'ouvrir un lot, vérifier qu'aucun lot déjà ouvert ne touche les mêmes
fichiers : deux lots qui modifient le même fichier n'entrent en conflit qu'au
moment de la RC, c'est-à-dire au pire moment. S'y rattacher plutôt qu'en créer un
second.

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

### Deux tags morts, à ne jamais republier

**`v0.9.86` et `v0.9.87` existent, et aucun des deux n'a jamais été publié.**
Les testeurs sont restés en 0.9.85 pendant toute cette période. Les tags sont
**conservés à dessein** : ils portent la mémoire de deux pannes qui ont chacune
laissé un garde-fou encore en service.

| tag | mort de quoi | ce qui en est resté |
|---|---|---|
| `v0.9.86` | build Windows : le module d'appairage AirPlay 2 était compilé sans condition alors que ses six dépendances sont déclarées sous `[target.'cfg(unix)'.dependencies]` | `#[cfg(unix)]` sur `pub mod pairing;` (#1933). La même famille a bloqué la **v0.9.130** trois mois plus tard — voir #3116. |
| `v0.9.87` | `apt-get` bloqué **six heures** sur un runner Ubuntu, jusqu'au plafond GitHub. Six jobs sur sept étaient verts, Windows compris : le code était bon, c'est l'infrastructure qui a lâché. `githubstatus.com` affichait « All Systems Operational ». | garde-fou apt (#1937) : 3 essais bornés, cache purgé entre deux, abandon en ~6 min avec un `::error::` qui nomme la cause. |

⛔ **Ne jamais republier ces deux tags, ne jamais les supprimer.** Ils sont
protégés par le gel `refs/tags/v*`, qui interdit création, modification **et**
suppression, sans aucune dérogation.

⚠️ **Si un job échoue sur `apt` : ce n'est probablement pas votre code.**
Vérifiez si d'autres exécutions Ubuntu échouent au même endroit au même moment,
et attendez la reprise plutôt que de relancer en boucle.

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

## 5. Notes de version multilingues

Arbitrage de #3089 : les notes de version sont **traduites à la publication**,
et `GET /system/changelog` sert la langue demandée (`?lang=xx`, sinon
`Accept-Language`, sinon `fr`). Ni français assumé, ni traduction à la volée.

### Format : un bloc par langue dans le corps de la release GitHub

Le corps de la release reste un seul texte Markdown. Le **français vient en
premier, sans marqueur**, exactement comme il a toujours été écrit. Chaque
traduction suit, ouverte par un commentaire HTML **seul sur sa ligne** :

```markdown
## Nouveautés
- …
## Corrections
- …

<!-- lang:en -->
## Features
- …
## Bug fixes
- …

<!-- lang:de -->
## Neuheiten
- …
```

Pourquoi ce format plutôt que des fichiers `notes-xx.md` attachés à la release :
le corps est déjà ce que le serveur télécharge — via le proxy `mozaiklabs.fr`
puis l'API GitHub —, donc **aucun appel réseau de plus** par langue ni par
release (vingt releases × dix langues auraient fait deux cents téléchargements
d'actifs, sur un dépôt privé qui exige un jeton). Le commentaire HTML est
invisible sur la page GitHub, traverse le proxy comme du texte, et se produit
avec le même `gh release edit vX.Y.Z --notes-file notes.md` que le français.

### Règles de lecture côté serveur (`tune-server/src/routes/system/update.rs`)

- Marqueur : `<!-- lang:xx -->`, espaces et casse tolérés, région ignorée
  (`<!--lang:EN-GB-->` vaut `en`). Tout autre commentaire HTML est du texte.
- Une release **sans aucun marqueur** est entièrement française : les releases
  antérieures à ce format se lisent sans changement.
- La langue demandée est cherchée d'abord ; à défaut, le bloc `fr`. La réponse
  le **dit** : `lang` est la langue effectivement servie et `fallback: true`
  signale qu'au moins une entrée n'a pas pu l'être dans la langue demandée.
  Chaque entrée porte ses propres `lang` et `fallback`, car une release
  ancienne peut côtoyer une release traduite dans la même liste.
- Un bloc vide (marqueur laissé sans texte) ne couvre pas sa langue : elle
  replie sur le français.

### Titres de rubriques reconnus

Le panneau ne connaît que trois rubriques ; les puces d'une section dont le
titre n'est pas reconnu sont **ignorées** (c'est voulu : « Mise à jour »,
« Téléchargements »). Les blocs traduits doivent donc employer l'un de ces
titres — le lecteur et cette table sont la même liste
(`TITRES_CORRECTIONS`, `TITRES_AMELIORATIONS`, `TITRES_NOUVEAUTES`) :

| Langue | Nouveautés | Améliorations | Corrections |
|---|---|---|---|
| fr | Nouveautés / Ajouts | Améliorations | Corrections |
| en | Features / New features | Improvements | Bug fixes / Fixes |
| de | Neuheiten / Neue Funktionen | Verbesserungen | Fehlerbehebungen / Korrekturen |
| es | Novedades / Nuevas funciones | Mejoras | Correcciones |
| it | Novità / Nuove funzioni | Miglioramenti | Correzioni |
| ro | Noutăți | Îmbunătățiri | Corecturi / Remedieri |
| sv | Nyheter / Nya funktioner | Förbättringar | Rättningar / Buggfixar |
| zh | 新功能 / 新增 | 改进 / 优化 | 修复 |
| ja | 新機能 | 改善 | 修正 / バグ修正 |
| ko | 새로운 기능 / 신규 | 개선 | 수정 / 버그 수정 |

Seules les **puces** (`- `, `* `, `• `) deviennent des items ; la prose et les
titres n'en sont jamais. Une note faite de paragraphes sous des titres
thématiques (« Lecture », « Bibliothèque ») donne un panneau réduit à
« Release x.y.z » — dans toutes les langues.

### Ce que le train doit produire

Une étape **outillée** de traduction, entre la rédaction française et la
publication du corps de la release, qui assemble `fr` + blocs `<!-- lang:xx -->`
dans un seul fichier passé à `gh release edit`. Les langues attendues sont
celles de `tune-server/src/i18n.rs::SUPPORTED`. Une langue manquante n'est pas
une panne : la route la replie sur le français en le disant. Ce document décrit
le contrat ; le branchement dans `release.yml` et le choix du traducteur sont
instruits dans #3089.

## Coordination des agents

OpenAI/Codex, Claude et les humains suivent le même circuit. Avant d'écrire,
un agent crée le label atomique `verrou:issue-N`, ajoute `en-cours` à l'issue
et indique dans la PR son fournisseur, son run, l'issue, la branche et le SHA
de base. Si la création échoue, l'agent vérifie le label exact : présent,
l'issue est prise ; absent, l'infrastructure est en erreur et l'agent s'arrête.

Une consigne locale peut renforcer ces règles, jamais autoriser un push direct
sur `main`/`rc/*`, un merge, un tag ou une publication.
