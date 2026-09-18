# Gestion du tag « compilation »

Plan de chantier, écrit le **14/09/2026** à la demande de Bertrand. Ce document dit ce qui
existe déjà, ce qui manque, ce qui revient à Bertrand, et une découpe en phases.

**Aucune ligne de code n'a été écrite pour ce document.** Tout ce qui suit est mesuré sur
`batch/bugs-13`.

## 1. Le symptôme, par trois testeurs

| issue | qui | ce qu'il voit |
|---|---|---|
| **#1656** | jfpaquet, 0.9.71 | les albums `VA-xxx` / `Various-xxx` portent un tag compilation ; le scan n'en tient pas compte et **met le titre de l'album comme artiste** |
| **#3855** | Pierre M, 0.9.147 | un coffret RCA de 63 CD : un dossier, six fichiers, **un** album chez MinimServer, **deux** albums de même titre chez Tune |
| **#3179** | jfpaquet, 0.9.130 | *Swinging Young Scott* attribué à Warren Vaché — en-tête **et** chaque ligne de piste |

Trois formes différentes, une même famille : **l'album ne sait pas qui est son artiste quand
les pistes n'ont pas toutes le même.**

## 2. Ce qui existe déjà — et c'est plus qu'on ne croit

### Le tag EST lu

`tune-core/src/metadata/mod.rs` :

```rust
pub compilation: bool,                                   // l. 82
"TCP" => "TCMP",                                         // l. 860, alias ID3v2.2
let compilation_str = tags.get("TCMP").unwrap_or("");    // l. 1786
let compilation = matches!(compilation_str, "1" | "true" | "True");
```

### Il est transporté jusqu'au scan

`tune-server/src/auto_scan.rs:58` et `:1772`, `scan_import.rs:511`, `:606`, `:691`,
`routes/library/albums.rs:120`. Le champ traverse la chaîne.

### La base a la colonne

Migration `albums_is_compilation` (`migrations.rs:1060`), lue par index 24 dans
`album_repo.rs`, écrite à l'`INSERT`. Et `mark_compilation()` existe (`album_repo.rs:1423`).

### Un module de regroupement existe

`tune-core/src/scanner/compilation.rs` sait reconnaître deux dossiers d'album qui sont
« les éclats possibles d'un même disque » — même nom, parents différents, même grand-parent.
C'est la forme que produit un rangement par artiste.

Plus une migration `merge_scattered_compilations` (`migrations.rs:937`) pour les
bibliothèques déjà indexées.

## 3. ⚠️ Ce qui manque — le point unique à établir

Il y a **deux sources de vérité concurrentes**, et rien ne dit laquelle gagne :

1. **le tag `TCMP` du fichier** — lu, transporté, jamais confronté au reste ;
2. **la forme des dossiers** — le module `scanner::compilation`, qui devine.

### 🔴 DÉMENTI — « `mark_compilation()` n'a aucun appelant de production »

> *Ce paragraphe disait : « **`mark_compilation()` n'a aucun appelant de production
> identifié.** Toutes les occurrences relevées sont soit sa définition, soit des essais
> (`album_repo.rs:3984`, `:4022` à `:4024`), soit un appel interne l. 1454. » — et il en
> tirait que le tag serait « lu de bout en bout et jeté à l'arrivée », ce qui expliquerait
> #1656 mot pour mot.*

**C'est faux.** Sur `origin/main` (`bcba767b`, v0.9.149), le drapeau est levé par **trois**
chemins de production, dont deux distincts au niveau appelant :

| appelant | chemin | ce qu'il couvre |
|---|---|---|
| `tune-server/src/scan_import.rs:933` | `self.album_repo.mark_compilation(aid)` | le scan par lots — manuel et au démarrage |
| `tune-server/src/auto_scan.rs:207` | `album_repo.mark_compilation(aid)` | le **surveillant de fichiers** |
| `tune-server/src/scan_import.rs:956` | `reclasser_en_compilation(aid, va, titre)` → `album_repo.rs:1454` → `mark_compilation` | la reprise d'une ligne née sous une décision partielle (#3232) |

Les deux premiers sont gardés par le même `if let Some(aid) = album_id && is_compilation`.
Le troisième est la voie #3232, qui réécrit artiste **et** titre avant de lever le drapeau.

👉 **Conséquence sur le diagnostic** : le tag n'est pas « jeté à l'arrivée ». Il est jeté
**bien avant**, dans le calcul de `is_compilation` lui-même — voir la mesure de la phase 1
ci-dessous. La colonne, elle, est écrite fidèlement à la décision prise.

⚠️ La leçon de méthode que ce paragraphe énonçait reste entière, et elle vient de se
retourner contre lui : `git grep` ne fait autorité ni sur l'atteignabilité, **ni sur
l'inatteignabilité**. Les trois appels ci-dessus étaient visibles d'un `grep` — c'est le
relevé qui avait été incomplet, pas le code qui était mort.

## 4. Ce qui revient à Bertrand

**C1 — Qui gagne, du tag ou de la forme des dossiers ? ✅ TRANCHÉ**

**Bertrand, 14/09/2026 : le tag fait foi, la forme sert de repli.** Le tag est une intention
explicite de celui qui a étiqueté ; la forme des dossiers n'est qu'une déduction. Quand les
deux se contredisent, **le tag l'emporte**, et la forme n'intervient que si aucun tag n'existe.

*Question d'origine, pour mémoire :*
Un fichier peut porter `TCMP=1` dans un rangement classique, et un coffret peut n'avoir aucun
tag. Trois postures : le tag fait foi et la forme ne sert que de repli ; la forme fait foi ;
ou l'un **confirme** l'autre et un désaccord se journalise sans trancher.
*Recommandation : le tag fait foi, la forme en repli — le tag est une intention explicite de
celui qui a étiqueté, la forme est une déduction.*

**C2 — Que devient l'artiste d'un album de compilation ? ✅ TRANCHÉ**

**Bertrand, 14/09/2026 : l'artiste d'album tagué s'il existe, sinon « Various Artists ».**
On respecte ce que le fichier déclare dans `ALBUMARTIST`, et on ne retombe sur la convention
que lorsqu'il est absent.

C'est le choix qui sert le coffret RCA Reiner de #3855 : 63 CD d'un seul chef d'orchestre, où
« Various Artists » serait une régression par rapport à l'étiquette du fichier.

*Question d'origine, pour mémoire :*
`Various Artists` en dur, l'artiste d'album tagué s'il existe, ou un champ nul que l'écran
interprète ? Cela décide de ce que voit l'utilisateur dans la liste des artistes — et de si
une compilation y crée une entrée parasite.

**C3 — Faut-il réparer les bibliothèques déjà indexées ?**
`merge_scattered_compilations` existe pour les albums éclatés. Mais un album marqué à tort,
ou un artiste faux comme dans #3179, n'a pas de rattrapage. Une passe de réparation touche
des données que l'utilisateur a pu corriger à la main : **ne jamais écraser un champ édité**
est la contrainte, et il faut savoir si on sait le distinguer.

**C4 — Le seuil d'un coffret.**
#3855 montre 63 CD sous un même parent. Combien de dossiers frères de même nom font une
compilation plutôt que deux éditions ? Le module en a une idée ; elle n'a jamais été
confrontée à un vrai coffret.

## 5. Découpe proposée

### Phase 0 — Établir ce qui est mort et ce qui vit

Sans toucher une ligne :

1. **prouver si `mark_compilation()` a un appelant de production.** La seule preuve qui vaut
   est la sortie d'un job, ou un journal sur une vraie bibliothèque — pas un `grep` ;
2. **mesurer, sur les fichiers de Bertrand, combien portent `TCMP=1`** et ce que Tune en a
   fait : combien d'albums sont `is_compilation = 1` en base aujourd'hui ?
3. **rejouer les trois cas des testeurs** : un dossier `VA-xxx` (#1656), le coffret RCA
   (#3855), et *Swinging Young Scott* (#3179) — les trois sont documentés avec leurs
   étiquettes.

**Témoin** : un tableau dans `docs/mesures/`, comme pour le chantier UPnP.
**Contre-épreuve** : un fichier `TCMP=1` scanné à neuf ; l'album est-il marqué ?

⚠️ **#1656 et #3179 portent `bloque:terrain`**, et #1656 porte aussi `keep-open`. Les
étiquettes de gel valent avant toute chose : on mesure, on ne ferme pas.

### Phase 1 — Le tag décide, et le dit

Brancher `metadata.compilation` sur `mark_compilation()` au scan, selon l'arbitrage **C1**.
Journaliser chaque décision : *cet album est une compilation parce que ses pistes portent
`TCMP=1`* — ou *parce que ses dossiers sont éclatés*. Aujourd'hui, rien ne le dit.

**Témoin** : un dossier `VA-xxx` tagué donne un album `is_compilation = 1` dont l'artiste
n'est pas le titre.
**Contre-épreuve** : le même sans tag ne le devient pas.

### Phase 2 — L'artiste d'une compilation

Appliquer **C2**. C'est la phase visible : c'est elle qui fait disparaître les artistes
parasites de la liste.

### Phase 3 — Les coffrets

#3855. Confronter le module de regroupement à un vrai coffret, et fixer le seuil de **C4**.

### Phase 4 — La réparation de l'existant

Selon **C3**, et seulement si C3 dit oui.

## 6. Ce que ce plan ne dit pas

- **Il ne sait pas si le défaut est encore présent.** Les trois signalements portent sur les
  versions 0.9.71, 0.9.130 et 0.9.147. La phase 0 commence par les rejouer sur la version
  courante — deux d'entre eux pourraient déjà être clos.
## 7. ✅ Phase 0 faite — la section précédente était FAUSSE

> Cette section affirmait, tableau à l'appui, que le tag n'était lu qu'en ID3 et que « sur une
> bibliothèque FLAC le tag compilation est ignoré à 100 % ». **C'est faux.** La réserve qui
> terminait la section — *« l'absence dans `metadata/mod.rs` ne prouve pas l'absence
> partout »* — était la bonne piste : la lecture vit ailleurs. Mesuré le 14/09/2026, #4145.

Le chemin principal est `mod.rs:2907`, qui demande `ItemKey::FlagCompilation` à lofty. Lofty
fait correspondre cette clé unique aux trois formes du tag :

| format | champ | lu ? |
|---|---|---|
| MP3 / ID3 | `TCMP` (alias `TCP`) | **oui** |
| FLAC, OGG (VorbisComment) | `COMPILATION`, casse indifférente | **oui** |
| M4A, ALAC (MP4) | atome binaire `cpil` | **oui**, normalisé en `"1"` / `"0"` |

La ligne `tags.get("TCMP")` (l. 1786) qui avait servi de base au constat n'est pas ce
chemin-là : c'est le **repli DSF/DFF**, qui analyse l'ID3 à la main faute de support lofty.

Sondé sur des fichiers réels du `.18`, branche **non modifiée** :

```
real_upper.flac (COMPILATION=1) -> compilation=true   lofty_brut=Some("1")
real_mixed.flac (Compilation=1) -> compilation=true   lofty_brut=Some("1")
real_cpil0.m4a  (cpil=0)        -> compilation=false  lofty_brut=Some("0")
real_cpil1.m4a  (cpil=1)        -> compilation=true   lofty_brut=Some("1")
```

Le `Some("0")` est la preuve que l'atome MP4 est **lu**, et non simplement absent.

### Ce que porte réellement la bibliothèque

| format | fichiers | tagués | valeurs rencontrées |
|---|---:|---:|---|
| FLAC | 24 937 | **1 260** (5,1 %) | `1`, et rien d'autre |
| M4A / ALAC | 82 | 10 | `cpil=0` — **aucun** `1` |
| MP3 | 140 | **0** | — |
| DSF | 735 | **0** | — |
| OGG / Opus | 0 | — | — |

Deux enseignements : **aucun encodeur n'écrit `yes` ni `true`** — la liste de valeurs n'avait
jamais rien raté — et **304 des 1 260 FLAC écrivent `Compilation`** et non `COMPILATION`, un
sur quatre, qui fonctionnait sans qu'aucune épreuve ne le couvre.

### Ce que #4148 a livré, du coup

Pas une correction de comportement : **il n'y avait pas de défaut à cet endroit**. Ce qui est
livré est un seul décodeur `lire_drapeau_compilation()` là où deux sites recopiaient la même
liste, la casse devenue indifférente (`TRUE` ne tombe plus), un journal qui **sépare absent de
faux** (`compilation_tag_absent` / `_faux` / `_vrai` / `_illisible`), et sept épreuves sur de
vrais conteneurs — il n'y en avait aucune. L'ensemble des valeurs rendues vraies est un
sur-ensemble strict de l'ancien : sur les 25 894 fichiers mesurés, pas un ne bascule.

👉 **Le défaut des trois signalements est donc en AVAL de la lecture**, pas dedans : le tag
arrive jusqu'à `metadata.compilation` et personne ne s'en sert. C'est la phase 1 — brancher
`mark_compilation()` — qui reste le premier correctif à écrire, et la question ouverte du § 3
(`mark_compilation()` a-t-il un appelant de production ?) devient la seule qui compte.

⚠️ Non mesurés : WAV et AIFF, 12 fichiers.

## 9. Phase 1 — ce qui décidait réellement, mesuré puis corrigé

Mesuré le 14/09/2026 sur `origin/main` `bcba767b` (**v0.9.149**), par le vrai `TrackImporter`,
le vrai `begin_batch` et une base SQLite neuve — les trois formes montées à l'identique des
signalements. Témoin : `tune-server/tests/compilation_le_tag_fait_foi.rs`.

### 9.1 Ce que faisaient les trois cas AVANT

| cas | forme jouée | `is_compilation` | artiste de l'album |
|---|---|---|---|
| **A** — #1656 | `VA-xxx/`, tag `1` sur les 3 pistes, aucun `ALBUMARTIST` | `true` | `Various Artists` |
| **B** — #3855 | 1 dossier de disque, 1 titre, **tag `0`**, 2 graphies du chef | `true` | `Various Artists` |
| **C** — #3855 | le même, **tag absent** | `true` | `Various Artists` |

👉 **#1656 était déjà réglée sur la .149** : le tag est honoré et le titre de l'album n'est plus
pris pour l'artiste (le point 2 l'avait été par `44111285`, v0.9.97). Le cas A ne change pas.

👉 **B et C rendaient le MÊME résultat.** C'est la mesure qui tranche : un fichier qui déclare
explicitement `COMPILATION=0` était **rigoureusement indiscernable** d'un fichier muet. Le tag
n'avait aucun effet observable dans ce sens.

### 9.2 Quelle règle décidait — et pourquoi le soupçon de départ était incomplet

Le soupçon portait sur l'ordre des `||` de `scan_import.rs:681`, `folder_va` passant devant le
tag. C'est vrai, mais ce n'est pas la cause suffisante, et s'y arrêter aurait manqué l'essentiel.

Le tag était **déjà fondu** dans le deuxième étage. `decide_compilation_albums` rendait
`flag || artists.len() >= 2`, où `flag` était mis à vrai par le tag. D'où :

1. un tag à **vrai** allumait le drapeau — par `comp_decision`, sans avoir besoin du `||` ;
2. un tag à **faux** n'avait **aucun chemin** vers la décision. `meta.compilation` n'était relu
   que dans le `unwrap_or_else`, c'est-à-dire pour un album **absent** de la carte — ce qui
   n'arrive jamais une fois `begin_batch` passé sur les pistes de cet album.

**Le tag ne pouvait donc qu'ALLUMER le drapeau, jamais l'éteindre.** La règle qui décidait
réellement, pour B comme pour C, était « deux artistes d'album distincts sous un même titre » —
la FORME — et deux graphies d'un même chef d'orchestre lui suffisent.

### 9.3 Ce que la phase 1 change

- `TrackMetadata::compilation` passe de `bool` à **`Option<bool>`** : sans trois états, C1 est
  inapplicable — « pas de tag » et « tag à zéro » étaient le même `false` ;
- `decide_compilation_albums` ne fond plus les deux sources : elle rend un [`VerdictAlbum`]
  où `tag` et `forme` restent **séparés** ;
- la décision s'écrit en un seul endroit et dans l'ordre de **C1** — le tag tranche dans les
  deux sens, la forme ne parle que s'il se tait ;
- **C2** : l'artiste d'un album de compilation est l'unique artiste d'album **tagué du
  DOSSIER** quand il n'y en a qu'un, `Various Artists` dès qu'il y en a deux ou plus ;
- le journal émet `compilation_decidee` une fois par album et par scan, avec `motif=tag` ou
  `motif=forme_des_dossiers`. **Rien ne le disait jusqu'ici.**

| cas | après | artiste |
|---|---|---|
| **A** | `true` — `motif=tag` | `Various Artists` (aucun artiste tagué) |
| **B** | **`false`** — `motif=tag` | **`Fritz Reiner`** |
| **C** | `true` — `motif=forme_des_dossiers` | `Various Artists` |

⚠️ **C3 est respecté : rien de déjà indexé n'est réparé.** `mark_compilation()` ne baisse
jamais le drapeau, et l'amorçage depuis la base entre en `Some(true)` pour une ligne déjà
marquée. Le correctif ne vaut que pour ce qui sera scanné **après**.

### 9.4 Le coffret de 63 CD (#3855) — ni mieux ni moins bien, et pourquoi

Mesuré : le coffret reste **UN seul album** après le changement. Ce n'est pas le drapeau
« compilation » qui le tenait groupé — c'est la réattache par titre identique de
`get_or_create` (`album_repo.rs:1327`). Retirer le drapeau ne le redécoupe donc pas, et
l'artiste s'améliore : `Various Artists` → `Fritz Reiner`.

Deux réserves, toutes deux de **phase 2**, à ne pas traiter ici :

1. la réattache journalise un `BUG_album_artist_mismatch` par piste divergente — bruit
   nouveau sur ce chemin, signe que les **deux graphies** du chef restent non normalisées ;
2. `COVER_DISTANCE_MAX` (`scanner/compilation.rs:103`) regroupe par empreinte de pochette,
   critère que **C4** condamne pour un coffret dont les 63 disques ont 63 pochettes. Ce lot
   n'y touche pas.

### 9.5 #3179 — hors de portée de C1/C2

*Swingin' Young Scott* attribué à Warren Vaché ne passe pas par le drapeau « compilation » :
un album jugé compilation s'afficherait sous `Various Artists`, pas sous un nom d'invité. Le
mécanisme est celui de l'artiste de repli quand `ALBUMARTIST` manque (`folder_tagged_artist`,
puis `dir_album_artist`). **Ni C1 ni C2 ne le déplacent** ; l'issue reste ouverte et son
`bloque:terrain` tient.

### 9.6 La portée de C2 — trois témoins rouges l'ont tranchée

C2 dit « l'artiste d'album tagué s'il existe ». **Sur quel périmètre** cet artiste doit-il
être unique ? La question n'est pas rhétorique : elle a trois réponses, et deux sont fausses.

| portée | ce qu'elle donne sur une compilation FAITE MAIN (#3232) | verdict |
|---|---|---|
| la **piste** courante | chaque piste son artiste ⇒ le coffret de #3855 se recoupe en deux lignes album, et « Woodstock » prend l'artiste de sa première piste | ❌ |
| l'**album** `(dossier, titre)` | chaque piste porte son propre titre d'album, donc chaque groupe n'a qu'une piste et donc qu'un artiste ⇒ une anthologie de vingt artistes sort sous « Aretha Franklin » | ❌ |
| le **dossier** | l'unique artiste d'album étiqueté du dossier, quand il n'y en a qu'un ; deux ou plus ⇒ `Various Artists` | ✅ |

La portée qui décide est celle **qui a fait la compilation** : le dossier. C'est
`folder_tagged_artist`, qui existait déjà pour les fichiers sans balises (#3232).

⚠️ Et **aucun repli sur la balise de la piste** quand cette valeur est absente : elle est
absente précisément quand le dossier porte plusieurs artistes, c'est-à-dire dans le seul cas
où aucune étiquette ne vaut pour tout l'album. Les cinq témoins rouges de l'étape
intermédiaire sont tous venus de ce repli, `un_coffret_dont_l_album_artist_varie_dans_un_
dossier_ne_se_coupe_pas_en_deux_3855` compris — le dépôt gardait déjà ce cas.

## 10. Phases 2 à 4 — livrées le 16/09/2026 (« RAF tag compilation : maintenant ! »)

Base `rc/v0.9.151` (qui porte la phase 1). Trois gestes, dans l'ordre des arbitrages.

### 10.1 C3 — le marqueur d'édition manuelle EXISTE

`album_metadata.edition_manuelle` : un tableau JSON trié de noms de champs (`artist`,
`title`, `genre`, `year`, `label`, et toute clé posée par `PUT /albums/{id}/metadata`).
Posé par les trois routes d'édition — `PUT /albums/{id}`, `PUT /albums/{id}/metadata`,
`POST /albums/batch` — au moment où l'utilisateur écrit. Cumulatif, idempotent, lisible par
`GET /albums/{id}/metadata` sans route dédiée. Le marqueur lui-même ne se force pas par
la route « metadata » : il dit ce que l'utilisateur a tenu, il n'est pas une valeur qu'il
tient.

Pas de colonne, pas de migration : le magasin clé-valeur existe sur les deux moteurs.

### 10.2 Phase 4 — la réparation de l'existant, gardée par C3

`GET|POST /library/compilations/reparation` (`routes/library/reparer_compilations.rs`),
tâche de fond au registre (#2129), 409 si déjà en cours.

Pour chaque album à pistes locales, **sauf ceux dont `artist` ou `is_compilation` est
tenu par une édition manuelle** :

1. relire le tag `compilation` et l'artiste d'album **dans les fichiers**
   (`read_metadata`, le lecteur du scan — la base ne garde que le verdict d'alors) ;
2. C1 : un tag qui parle tranche **dans les deux sens** (un vrai l'emporte sur un faux) ;
   sinon la forme (un « Various Artists », ou deux artistes d'album distincts dans un même
   dossier) ;
3. C2 : compilation ⇒ l'unique artiste d'album tagué, sinon « Various Artists » ; pas
   compilation ⇒ l'unique artiste tagué s'il y en a exactement un, sinon on n'invente rien ;
4. n'écrire que si quelque chose change — `AlbumRepo::reparer_compilation`, **la seule porte
   par laquelle le drapeau peut baisser hors d'un rescan complet** — et le journaliser
   (`compilation_reparee`, avec `motif`).

Un album dont aucun fichier n'est lisible n'est pas jugé. Le bilan (`repaired`,
`unchanged`, `manual_skipped`, `unreadable`, `errors`) reste lisible après coup.

Mesuré sur fixture réelle : le coffret de #3855 tel que l'ancienne règle l'a laissé en base
(« Various Artists », drapeau levé, fichiers à `COMPILATION=0`) ressort **drapeau baissé,
artiste `Fritz Reiner`** ; une seconde passe n'a plus rien à faire ; le même album marqué
édité à la main n'est pas touché.

⚠️ Pas encore de bouton dans le client : la route existe, l'écran suit.

### 10.3 C4 — un coffret rangé en dossiers de disques fait UN album

La forme mesurée d'un coffret est `Coffret/CD01/…`, `Coffret/CD02/…`. Elle tombait dans
« autre dossier = autre édition » (`get_or_create_for_folder_with_track`) : un album par
disque. Désormais, trois faits ENSEMBLE rattachent `CD02` au coffret que `CD01` a ouvert :

- même titre et même artiste (c'est le candidat déjà trouvé) ;
- deux **dossiers de disques frères** sous le même parent —
  `scanner::compilation::sont_des_disques_du_meme_coffret` : un mot de la famille « disque »
  (`cd`, `disc`, `disk`, `disque`, `vol`, `volume`) puis un nombre, en tête du nom ;
- un **numéro de disque que l'album n'a pas encore** (`disc_numbers_of`). C'est ce qui
  sépare un coffret de deux extractions identiques rangées côte à côte : elles se
  disputent le disque 1.

La pochette n'entre pas dans la règle : 63 disques, 63 pochettes — `COVER_DISTANCE_MAX`
reste ce qu'il est pour l'autre forme (rangement par artiste, `is_scattered_sibling`), les
deux formes ne se recouvrent jamais.

Contre-épreuves tenues : seconde extraction (disque 1 déjà pris) ⇒ sa ligne ; frère sans nom
de disque ⇒ sa ligne ; nom de disque sans numéro de disque dans les balises ⇒ sa ligne ;
`/CD01` et `/CD02` à la racine ⇒ rien ne les relie.

### 10.4 Ce qui reste

- **Les deux graphies du chef** (réserve 1 de 9.4) : le bruit `BUG_album_artist_mismatch`
  vient de deux `artist_id` demandés pour un même dossier. Ce n'est pas un défaut visible —
  la ligne album reste une — mais une normalisation des graphies d'artiste à
  `get_or_create` reste à faire. Hors de ce lot.
- **Le client** : bouton « Réparer les compilations » (Métadonnées) et pastille « édité à la
  main » sur la fiche album. La phase 5 (§11) y ajoute le bouton « Compilation » de la barre
  de sélection, la gravure et la pastille — voir renesenses/tune-web-client#1172.
- **#3179** reste hors de portée de ce chantier (artiste de repli, pas drapeau).

## 11. Phase 5 — la main de l'utilisateur (18/09/2026, #4427)

Demande de Bertrand, capture à l'appui : la compilation **Coco María Presents** occupe douze
vignettes, une par artiste de piste. Il veut cocher ces albums dans l'écran Métadonnées et
poser le drapeau dessus.

Les phases 1 à 4 avaient donné au scan de quoi bien décider. Il manquait de quoi le
**contredire**.

### 11.1 Les trois arbitrages du 18/09

- **Portée** : la base retient le choix tout de suite, et graver dans les fichiers est une
  **seconde action, explicite**. L'utilisateur doit savoir quand Tune touche à ses fichiers.
- **Regroupement** : poser le drapeau sur plusieurs albums propose de les réunir en un seul
  disque — c'est le résultat attendu de la capture. Le décochage, lui, ne défait aucune
  fusion.
- **Unité** : on coche des **albums**, dans la barre de sélection qui sert déjà à l'artiste et
  au genre. Pas de case par piste : douze fiches à ouvrir pour un geste unique.

### 11.2 Le marqueur existait ; personne ne le lisait au scan

`album_metadata.edition_manuelle` est posé depuis le 16/09 (§10.1) et la phase 4 le respecte.
Mais `git grep champs_edites_a_la_main` ne rendait **qu'un** appelant : la passe de
réparation. `mark_compilation` et `reclasser_en_compilation` passaient outre — et le drapeau
ne sachant que MONTER, un album décoché à la main était recoché dès le fichier suivant vu par
le surveillant.

La garde est désormais dans le dépôt (`AlbumRepo::tenu_a_la_main`), pas chez les appelants :
le scan par lots et le surveillant écrivent tous deux, et un troisième appelant écrira un
jour. Une lecture en échec rend « personne n'a tranché » — le scan n'est jamais bloqué par une
table de métadonnées illisible.

### 11.3 Les deux routes

- `PUT /albums/{id}` et `POST /albums/batch-update` acceptent `is_compilation`. Absent veut
  dire « je n'y touche pas », jamais « faux » : on édite le titre d'un album sans lui reprendre
  son drapeau. L'écriture passe par `reparer_compilation` — `repo.update()` ne porte pas le
  drapeau, délibérément — et le marqueur est posé dans la foulée.
- `POST /library/albums/compilation/graver` (`routes/library/graver_compilation.rs`) écrit
  `1` ou `0` sous `ItemKey::FlagCompilation`, la clé exacte que le scan relit. **`0` est
  écrit, pas effacé** : depuis C1, « pas de tag » et « tag à zéro » ne disent pas la même
  chose, et c'est le `0` explicite qui empêche la forme des dossiers de reprendre la main sur
  une compilation refusée. Un conteneur que le scan ne relit pas (WAV, DSF, DFF, Matroska) est
  compté à part (`hors_format`), jamais gravé — même règle que `graver_dr`. Pas de tâche de
  fond : la sélection est faite à la main, quelques centaines de fichiers au pire.

Aucune fusion d'albums côté serveur : `POST /library/albums/merge` existe depuis longtemps,
c'est le client qui l'enchaîne.

### 11.4 Contre-épreuves

Cinq points cassés un par un, cinq rouges : `mark_compilation` sans garde ;
`reclasser_en_compilation` sans garde ; le lot qui n'écrit pas le drapeau ; le lot qui ne
marque pas le champ tenu ; la gravure qui efface au lieu d'écrire `0`. La gravure est mesurée
sur un **vrai FLAC**, relu par `read_metadata` — le lecteur du scan, pas une réplique.

### 11.5 Ce qui reste après la phase 5

- **L'écran**, suivi par renesenses/tune-web-client#1172 : le bouton de la barre de sélection,
  l'enchaînement de la fusion, la gravure et la pastille.
- **Le bouton « Réparer les compilations »** de la phase 4, toujours sans écran.
