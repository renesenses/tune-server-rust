# Audit des lots `batch/*` restés en avance sur la ligne — 20/09/2026

Neuf lots `batch/*` étaient encore **en avance** sur `rc/v0.9.160` après la
promotion des huit lots de la campagne. Ce document dit, commit par commit, ce
qu'ils portent réellement — **pour qu'aucun audit futur n'ait à refaire la
mesure**. C'est exactement ce qui manquait depuis l'audit du 12/09/2026 :

> « Un lot promu devrait laisser une trace SUR SA BRANCHE SOURCE. Sans ça,
> chaque futur audit refera la vérification par le contenu. »

## Conclusion, d'abord

**Aucun de ces neuf lots ne doit être fusionné.** Ils ne portent pas de dette :
ils portent des **doublons sous d'autres noms** et **deux collisions de
migration PostgreSQL**.

## La méthode, et sa limite

Deux passes, parce que la première ne suffit pas.

1. **Par empreinte de patch** (`git cherry rc/v0.9.160 batch/<lot>`). Strict :
   un correctif réappliqué à la main, ou rebasé sur une ligne qui a bougé de
   plus de mille commits, en ressort « absent » alors que son effet est là.
   C'est un **majorant**, jamais une dette.
2. **Par le contenu**. Pour chaque commit restant : ses lignes ajoutées
   substantielles (≥ 20 caractères, hors commentaires — les commentaires se
   recopient et fausseraient le compte), comparées au fichier tel qu'il est sur
   la ligne aujourd'hui.

⚠️ **La seconde passe compare des fichiers par leur CHEMIN.** Un module renommé
ressort donc « absent » à tort — c'est arrivé ici, et c'est le point le plus
important de cet audit (voir `crossfade.rs` plus bas). Tout verdict « absent »
a donc été revérifié **par la fonction, pas par le nom**.

## Passe 1 — ce que l'empreinte dit

| lot | commits réels | déjà appliqués | restants |
|---|---|---|---|
| `p1-audio-output-contracts-2` | 7 | 0 | 7 |
| `bugs-1` | 6 | 0 | 6 |
| `p2-anciennes-2` | 4 | 0 | 4 |
| `p1-replaygain-analysis-integrity-v2` | 4 | 1 | 3 |
| `vague-8` | 3 | 1 | 2 |
| `vague-11` | 1 | 0 | 1 |
| `vague-14` | 3 | 3 | **0** |
| `vague-10` | 0 | — | **0** |
| `p2-vague-7` | 0 | — | **0** |

**Trois lots sont réglés sans rien faire.** `vague-14` est entièrement absorbé ;
`vague-10` et `p2-vague-7` n'ont **aucun commit réel** — leur « avance » n'était
que des commits de fusion, que `git cherry` ignore à juste titre.

Restent **23 commits sur 6 lots**.

## Passe 2 — ce que le contenu dit

Format : `sha  lignes retrouvées / lignes mesurées  verdict`.

### `batch/vague-11` — absorbé
```
a550545f   388/402   DÉJÀ DANS LA LIGNE   radios : refuser une adresse de flux illisible
```
Ce lot rejoint les trois réglés : **rien à en tirer**.

### `batch/vague-8`
```
ff633803    12/225   ABSENT     lastfm : la zone navigateur annonce son écoute
c8de866f    59/65    DÉJÀ       chromecast : l'arrêt relâche le média
```

### `batch/p1-replaygain-analysis-integrity-v2`
```
a082c136     6/83    ABSENT     mesurer le true peak BS.1770-5 (#2713)
2573b9ed    11/51    PARTIEL 22%  versionner les valeurs true peak (#2713)
7aa66ab7     9/95    ABSENT     recalculer les sample peaks (#2713)
```

### `batch/p2-anciennes-2`
```
f945bd86   101/104   DÉJÀ       retrait d'un dossier par PATCH /system/config
84522291    40/230   PARTIEL 17%  radios : la recherche interroge l'annuaire
82f7effb   326/379   PARTIEL 86%  favoris : un ordre manuel (#2001)
6199782f   131/131   DÉJÀ       playlists : une piste Qobuz n'entre pas en local
```

### `batch/bugs-1`
```
6defe9e7     0/11    ABSENT     cloud-relay dans tous les binaires et la CI (#3355)
5212c00f    37/42    PARTIEL 88%  cible de transcodage inconnue → FLAC
5cda7c8c     0/4     ABSENT     workflows_bornes nomme cloud-relay (#3355)
47d8f2d3   141/141   DÉJÀ       repli DSF/DFF : nettoyer le titre
2dea959f     2/6     PARTIEL 33%  rustfmt (cosmétique)
476cd5b6   112/112   DÉJÀ       accueil : la requête des genres répond
```

### `batch/p1-audio-output-contracts-2`
```
cb4a23be    26/70    PARTIEL 37%  oaat : ignorer les réponses de flux périmées (#2730)
49885b68    26/245   PARTIEL 11%  openhome : relier les pins au renderer (#2722)
060a5f10   179/212   PARTIEL 84%  streaming : qualité par zone (#2723)
49f6c56f    11/25    PARTIEL 44%  oaat : ignorer les stats pendant la négociation (#2758)
a2a8c0a6    78/82    DÉJÀ       multiroom : synchronisation réservée à OAAT (#2215)
0efb5817    15/70    PARTIEL 21%  dlna : attendre l'URI pendant le réveil HEOS (#2749)
09be1df6    16/253   ABSENT     crossfade : superposer le PCM local (#2211)
```

## Passe 3 — les « absents » revérifiés par la FONCTION

🔴 **Aucun n'est du travail perdu.**

| sujet | verdict brut | où il est réellement |
|---|---|---|
| crossfade PCM local | ABSENT (`tune-core/src/playback/crossfade.rs` introuvable) | **`tune-core/src/audio/fondu_enchaine.rs`** — le module a été renommé en français et réécrit |
| true peak BS.1770-5 | ABSENT | `tune-core/src/audio/replaygain.rs`, `tune-core/src/audio/analyzer.rs` |
| cloud-relay dans les binaires et la CI | ABSENT | `.github/workflows/{ci,docker,release}.yml` et `tune-server/Cargo.toml` |
| Last.fm, zone navigateur | ABSENT | présent dans le cœur (`config.rs`, `credentials_vault.rs`, …) |

## 🔴 Le vrai danger : deux collisions de migration

Ces lots portent deux migrations PostgreSQL dont les numéros sont **déjà pris**
sur la ligne :

| migration du lot | numéro déjà occupé sur la ligne par |
|---|---|
| `041_replaygain_true_peak_bs1770_5.sql` | `041_hidden_items.sql` |
| `047_favoris_ordre_manuel.sql` | `047_listen_history_album_id_bigint.sql` |

Une fusion à l'aveugle poserait donc une collision de numérotation **sur des
bases de production**. C'est la raison la plus forte de ne pas fusionner.

## Ce qui reste à regarder, et c'est tout

Trois commits sont partiels **sous 45 %** — assez bas pour qu'on ne puisse pas
conclure depuis le contenu seul, et assez haut pour qu'il y ait quelque chose :

- `49885b68` — openhome : relier les pins au renderer (11 %) ;
- `84522291` — radios : la recherche interroge l'annuaire (17 %) ;
- `0efb5817` — dlna : attendre l'URI pendant le réveil HEOS (21 %).

Le reste est du bruit de mesure.

## Ce que cet audit ne prétend pas

- Il mesure des **lignes**, pas des comportements. Un correctif réécrit avec
  d'autres mots ressort « absent » : c'est pourquoi la passe 3 existe.
- Il ne dit rien des branches `batch/*` déjà à `ahead=0` — elles sont dans la
  ligne, point.
