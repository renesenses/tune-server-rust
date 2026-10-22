# #4805 — les introuvables de la mesure A, classés par cause, puis rejoués

Mesuré le 13/10/2026 sur Shrek, sur une **copie** de la base du 23/09/2026 (celle de
`4805-mesure-a-identify-all.md`). Rien n'a été lancé sur un serveur en service.

## 1. La mesure A rejouée, avec le verdict de chaque album

La première mesure A ne gardait pas le verdict par album. Elle a été rejouée le 13/10
avec le journal (`mesure_a::noter`), sur une copie fraîche, en 2 h 15 :

| | 1re mesure (13/10 matin) | rejouée (13/10 après-midi) |
|---|---:|---:|
| identifiés | 2 099 | 2 001 |
| ambigus | 467 | 449 |
| introuvables (MusicBrainz a répondu « rien ») | 1 343 | **919** |
| refus de MusicBrainz (503) | 0 | **540** |

Les deux passes ne se comparent pas terme à terme : la seconde a pris 540 refus,
partagés avec d'autres requêtes sorties de Shrek au même moment (même IP). Un refus
ne dit rien de l'album ; il est compté à part. Le classement porte donc sur les
**919 introuvables réels** de la seconde passe.

## 2. Les causes (`diagnostic_des_introuvables`)

Deux sondes MusicBrainz par album, au limiteur partagé : le titre seul (sans
artiste) rend-il un pressage du même titre ? Sinon, l'artiste seul existe-t-il ?

| cause première | albums | dont |
|---|---:|---|
| **titre** : l'artiste existe, pas ce titre | 367 | marque d'édition 105, **numéro de disque 88**, caractères 41, autre 133 |
| **artiste** : le titre existe sous un autre crédit | 262 | autre artiste ou homonyme 207, **accents absents de la balise 36**, crédit plus long 14, **article en fin de nom 5** |
| **compilation** | 177 | **artiste fictif (`Unknown Artist`) 113**, `Various Artists` 64 |
| indéterminé (sonde refusée) | 80 | |
| absence (ni le titre ni l'artiste) | 33 | |

Quatre causes se corrigent dans la REQUÊTE seule, sans toucher la donnée :

1. **Numéro de disque nu** (109 albums, toutes causes) : `Histoires CD1`,
   `Three Sides Live CD 2`, `The Song Remains The Same (CD 1)`. Le nettoyage
   existant ne retirait que `, Disc 1` et ` - CD 2`.
2. **Artiste fictif** (113) : `artist:"Unknown Artist"` ne ramène rien. Sans
   meilleur nom (les pistes non plus), la requête porte le titre seul. Garde : un
   titre fictif (`Unknown Album`, `Disque 1`, `Untitled`) n'est alors pas cherché du
   tout.
3. **Accents** (36) : `Etienne Daho` contre `Étienne Daho`, `Celine Dion`,
   `Telephone`. La recherche rendait le pressage ; le filtre `plausible` l'écartait.
   Il compare désormais aussi sans diacritiques.
4. **Article en fin de nom** (5) : `Brecker Brothers, The` s'interroge
   `The Brecker Brothers`.

Le choix (`choix_de_pressage` : score, départage, complétude des pistes) juge
ensuite comme pour tout album : un pressage incertain reste **ambigu**, rien n'est
écrit.

## 3. Rejeu des 919 introuvables avec les corrections (`rejeu_des_introuvables`)

Même copie, même boucle que la route (`executer_le_lot`), 30 min.

| sous-cause | albums | identifiés | ambigus | introuvables | refus |
|---|---:|---:|---:|---:|---:|
| artiste fictif | 113 | **47** | 23 | 21 | 22 |
| numéro de disque | 108 | **29** | 21 | 16 | 42 |
| accents (artiste) | 69 | **25** | 2 | 29 | 13 |
| article en fin de nom | 6 | **3** | 2 | 1 | 0 |
| `Various Artists` | 64 | 0 | 0 | 44 | 20 |
| autres (requête inchangée) | 559 | 30 | 11 | 437 | 81 |
| **total** | **919** | **134** | **59** | **548** | **178** |

| sur la copie | avant le rejeu | après |
|---|---:|---:|
| albums locaux identifiés | 2 030 | **2 164** (+134) |
| albums avec `release_group_id` | 2 001 | **2 135** |
| pistes locales avec `musicbrainz_recording_id` | 22 370 | **23 743** (+1 373) |
| artistes avec MBID (étape B) | 979 | **998** |

Lecture :

- **104 albums** identifiés dans les quatre sous-causes visées (sur 296), et 48
  ambigus de plus, que le départage ou la fiche peuvent trancher.
- Les **30 albums « autres »** identifiés alors que leur requête n'a pas changé sont
  le bruit du jour (index MusicBrainz, réponses différentes d'une heure à l'autre) :
  6,3 % des 478 répondus. C'est l'ordre de grandeur de l'incertitude de ce rejeu.
- `Various Artists` : 0. Ce sont des compilations dont MusicBrainz ne connaît pas
  ce titre ; aucune réécriture de requête n'y peut rien.
- **178 refus** encore. Une nouvelle passe les reprendrait (`identification_tentee_le`
  les range derrière les albums jamais tentés).

## Rejouer

```text
# 1. la mesure A, avec le verdict par album (voir 4805-mesure-a-identify-all.md)
# 2. le diagnostic
HOME=<dossier>/home TUNE_MESURE_A_RESULTAT=<dossier>/resultat.json \
TUNE_MESURE_A_DIAGNOSTIC=<dossier>/diagnostic.json \
  <binaire de test> --ignored --exact --nocapture \
  routes::library::identification_lot::mesure_a::diagnostic_des_introuvables
# 3. le rejeu, sur une copie de la copie
HOME=<dossier>/home TUNE_MESURE_A_DB=<dossier>/base/rejeu.db \
TUNE_MESURE_A_RESULTAT=<dossier>/resultat.json TUNE_MESURE_A_REJEU=<dossier>/rejeu.json \
  <binaire de test> --ignored --exact --nocapture \
  routes::library::identification_lot::mesure_a::rejeu_des_introuvables
```
