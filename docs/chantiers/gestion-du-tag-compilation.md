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

Et surtout : **`mark_compilation()` n'a aucun appelant de production identifié.** Toutes les
occurrences relevées sont soit sa définition, soit des essais (`album_repo.rs:3984`, `:4022`
à `:4024`), soit un appel interne l. 1454.

👉 **C'est la vérification n° 1 de la phase 0** : si le drapeau n'est jamais levé hors essai,
alors le tag est lu de bout en bout **et jeté à l'arrivée** — ce qui expliquerait #1656 mot
pour mot. À prouver, pas à supposer : `git grep` ne fait pas autorité sur l'atteignabilité
(cf. la leçon de `feedback_la_version_testee_nest_pas_celle_qui_tourne`).

## 4. Ce qui revient à Bertrand

**C1 — Qui gagne, du tag ou de la forme des dossiers ?**
Un fichier peut porter `TCMP=1` dans un rangement classique, et un coffret peut n'avoir aucun
tag. Trois postures : le tag fait foi et la forme ne sert que de repli ; la forme fait foi ;
ou l'un **confirme** l'autre et un désaccord se journalise sans trancher.
*Recommandation : le tag fait foi, la forme en repli — le tag est une intention explicite de
celui qui a étiqueté, la forme est une déduction.*

**C2 — Que devient l'artiste d'un album de compilation ?**
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
## 7. 🔴 Le fait le plus grave, vérifié — le tag n'est lu QU'EN ID3

`TCMP` est la forme **ID3**, celle des MP3. Les deux autres n'existent nulle part dans le
code :

| format | champ attendu | trouvé dans `metadata/mod.rs` |
|---|---|---|
| MP3 / ID3 | `TCMP` (alias `TCP`) | **oui**, l. 860 et 1786 |
| **FLAC, OGG** (VorbisComment) | `COMPILATION` | **NON** |
| **M4A, ALAC** (MP4) | `cpil` | **NON** |

La machinerie Vorbis est pourtant là — `raw_vorbis_comment()` et `read_vorbis_header()`
(l. 2620-2631) savent lire un champ arbitraire. **Personne ne leur demande `COMPILATION`.**

👉 **Conséquence : sur une bibliothèque FLAC — le cas de la plupart des testeurs
audiophiles — le tag compilation est ignoré à 100 %, quelle que soit la suite du chantier.**

C'est probablement le **premier** correctif à écrire, et il est court : demander le champ aux
deux autres formats, comme on le fait déjà pour ID3. Sans lui, les phases 1 à 4 ne
serviraient que les MP3.

⚠️ À confirmer tout de même en phase 0 : l'absence dans `metadata/mod.rs` ne prouve pas
l'absence **partout**. Chercher `COMPILATION` et `cpil` dans tout `tune-core/src` avant de
conclure — une lecture peut vivre dans le décodeur plutôt que dans l'extracteur d'étiquettes.
