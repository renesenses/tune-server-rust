# Le rééchantillonneur mesuré contre une référence indépendante — T10 de #2218

**État : mesuré, par des témoins qui rougissent, contre une référence prouvée
avant de servir.** Chaque chiffre de ce document est produit par un test de
`tune-core/tests/reechantillonnage_reference_2218.rs`, qui l'affirme dans son
message d'assertion. Le fichier tourne sur **toute PR Rust** (cible `[[test]]`
sans caractéristique requise, job `Test` de `ci.yml`) en **19 s** sur Shrek en
debug. Aucun fichier de production n'a été modifié : là où un témoin révèle un
défaut, il est nommé ici et dans un témoin `#[ignore = "défaut connu : …"]`
qui affirme le comportement attendu, à dé-ignorer par le correctif.

Relevé du 12/09/2026 sur `origin/batch/bugs-12` à `49ecf1fe`, rubato 3.0.0.

> **Mise à jour du 13/09/2026 — D1 et D2 sont corrigés.** Tout ce qui suit
> jusqu'à « Correctif du 13/09 — D1 » décrit l'état du 12/09, c'est-à-dire
> **avant** les correctifs ; il est conservé tel quel parce que c'est la mesure
> qui a motivé les changements. Les chiffres d'aujourd'hui, le coût CPU, la
> latence et ce qui reste ouvert sont dans
> « [Correctif du 13/09 — D1](#correctif-du-1309--d1-est-corrigé) » puis
> « [Correctif du 13/09 — D2](#correctif-du-1309--d2-est-corrigé) ».

> **Mise à jour du 15/09/2026 — D3 (#4079).** La somme de normalisation du
> noyau Rubato est désormais compensée. Les deux témoins encore ignorés sont
> exécutés et un témoin de gain continu est ajouté : **83/83 réussis** sur
> Shrek. Les relevés des 12/13 septembre ci-dessous restent historiques ;
> le nouveau relevé et le coût sont dans « Correctif du 15/09 — D3 ».

> **Mise à jour du 16/09/2026 — garde du noyau 1 024 (#4080).** La garde des
> 8 × 7 couples tourne bien en CI (preuve par le journal d'un run), deux
> témoins **mesurés** sur 352,8 et 384 → 44,1 kHz s'ajoutent au banc
> (**85/85**), et le coût du barreau 1 024 est chiffré sur Shrek — x86_64,
> pas Raspberry Pi. Voir « Garde du noyau 1 024 (#4080) » en fin de document.

## Pourquoi une référence

Le bilan du 12/09 : « le rééchantillonnage n'a aucune référence externe ; il
reste gardé par les empreintes de R1, contre la version d'avant, pas contre une
vérité ». Le banc AES17 (#3313, `docs/audio-conformance-aes17.md`) mesure un
résidu de Tune contre Tune ; les tests de `resample.rs` balaient jusqu'à
**18 kHz au plus** (`downsampling_stepped_sweep_preserves_passband_and_rejects_alias`).
Shrek n'a ni `sox` ni `ffmpeg` (`which sox ffmpeg` vide) : la référence est
écrite dans le test.

## La référence, et sa preuve contre elle-même

Interpolation sinc à fenêtre de **Kaiser β = 14** (réjection théorique
≈ 136 dB), **1 025 coefficients** (2 × 512 + 1), tout en `f64`, coupure à
`0,5 · min(1, ratio) − Δf/2` avec Δf la largeur de transition de Kaiser
(≈ 0,0087 cycle/échantillon d'entrée, soit ≈ 383 Hz à 44,1 kHz : la bande à
−0,1 dB de la référence dépasse 21,6 kHz sur tous les rapports). Pour un
rapport rationnel L/M (`vers/de` réduit), l'instant du n-ième échantillon de
sortie est exactement `n·M/L` : la table est **polyphase exacte** (L phases,
une par reste de `n·M mod L`), sans interpolation entre phases. Un décalage
fractionnaire `δ` (en trames de sortie) peut être intégré à la table pour
comparer Tune après retrait de son délai résiduel. Phase linéaire, délai nul.

| preuve | témoin | mesuré | seuil |
|---|---|---|---|
| sinus 1 kHz à −6 dBFS, 44,1 → 48 kHz : amplitude, phase, THD+N | `reference_sinus_1k_44_vers_48_thd_n_sous_moins_120_db` | amplitude 0,500000001, phase 3,4e−13 rad, **THD+N −153,7 dB** | < −120 dB |
| ton à 30 kHz, 96 → 48 kHz (au-dessus de Nyquist de sortie) | `reference_ton_au_dessus_de_nyquist_96_vers_48_rejete_sous_moins_120_db` | **−153,2 dB** | < −120 dB |
| gain continu (somme des coefficients), 7 rapports | `reference_gain_continu_unite` | 1 + 5,2e−9 | 1 ± 1e−7 |
| décalage δ = 0,37 trame retrouvé par la phase | `reference_decalage_fractionnaire_est_exact` | 0,37 ± 1e−6 | — |

C'est ce qui prouve que la référence mesure quelque chose : ce qu'elle rend
d'un sinus est un sinus à −154 dB près, ce qu'elle rejette est rejeté à
−153 dB, et l'outil qui mesure le délai de Tune est lui-même exact.

## Protocole

* Signaux synthétiques déterministes, une seconde, amplitude 0,5 (−6 dBFS) :
  sinus 1 kHz, 10 kHz, 20 kHz ; balayage linéaire 20 Hz → 20 kHz ; impulsion
  unité à 0,1 s (0,25 s de signal) ; ton de repliement (descente seulement).
* Rapports : ceux que Tune fait vraiment — 44,1 → 48, 48 → 44,1, 44,1 → 96,
  96 → 48, 44,1 → 192, **176,4 → 48** (sortie PCM de DSD64,
  `dsd_to_pcm::choose_output_rate`, vers une zone 48 kHz : le DSD ne passe par
  le SRC qu'après conversion PCM), 192 → 44,1 (seule branche `inv_ratio > 4`
  de `new_streaming_resampler`, noyau 512). Noyau rubato : 128 coefficients
  quand `de/vers ≤ 2`, 256 jusqu'à 4, 512 au-delà ; `oversampling_factor`
  256, interpolation linéaire, fenêtre `BlackmanHarris2`.
* Portes publiques : `rubato_resample_track` (piste d'un bloc, délai retiré,
  longueur exacte) et `new_streaming_resampler` + `rubato_resample_chunk`
  (le chemin du producteur : blocs de 1 024 puis 4 096 trames, puis `flush`).
* **Erreur RMS** : `RMS(Tune − référence) / RMS(référence)` en dB sur les 80 %
  centraux de la sortie (les bords sont mesurés à part). « Brute » = référence
  à délai nul ; « alignée » = référence décalée du délai résiduel mesuré.
* **Délai résiduel** : différence de phase entre Tune et la référence par
  ajustement exact `a·sin + b·cos` à 1 kHz, convertie en trames de sortie ;
  refaite à 10 kHz (égale ⇒ phase linéaire). Positif = Tune en retard.
* **Réponse en fréquence** : TFD directe de la réponse impulsionnelle de Tune
  (±4 096 trames autour du pic), normalisée par le rapport de cadences (un
  interpolateur à gain unité rend une impulsion de somme `vers/de`) ; bande à
  −0,1 dB = première fréquence sous −0,1 dB (pas 50 Hz) ; gain à 20 kHz
  **confirmé par un second avis indépendant** (ajustement d'un sinus à
  20 kHz) : les deux concordent à 1e−5 dB sur les 7 rapports.
* **Réjection** : montée — pire module des images entre le quart de la bande
  d'arrêt (`Nyq_in + 0,25·(Nyq_out − Nyq_in)`) et Nyquist de sortie ;
  descente — ton au quart de la bande d'arrêt, RMS de tout ce qui ressort (la
  référence, elle, rend −153 dB).
* **Bords** : erreur RMS sur les 64 premières et 64 dernières trames de la
  piste, référence alignée.
* **Vidage** : trames rendues par `flush` seul, trames de flux au-delà de
  `output_delay() + attendu`, et erreur des 64 dernières trames utiles.
* **Blocs / piste** : stéréo, blocs de 1 024 puis 4 096, délai retiré, contre
  la piste d'un bloc — écart absolu maximal.

## Les chiffres, par rapport

| rapport | noyau | délai résiduel (trames) | erreur RMS 1 kHz alignée / brute | THD+N Tune 1 kHz | erreur balayage (2–18 kHz) | gain 20 kHz | bande −0,1 dB | réjection | bords début / fin | `output_delay` / vidage / marge de queue |
|---|---|---|---|---|---|---|---|---|---|---|
| 44,1 → 48 | 128 | −0,344 | −108,4 / −26,9 dB | −140,2 dB | −94,6 dB | **−10,31 dB** | **18 550 Hz** | −109,8 dB (images) | −57,2 / −58,2 dB | 69 / 2 229 / 2 083 |
| 48 → 44,1 | 128 | −0,204 | −105,0 / −30,8 dB | −140,1 dB | −85,2 dB | **−9,90 dB** | **18 450 Hz** | −139,9 dB (repliement) | −57,8 / −61,4 dB | 58 / 1 882 / 998 |
| 44,1 → 96 | 128 | −0,689 | −108,4 / −26,9 dB | −139,8 dB | −94,6 dB | **−10,31 dB** | **18 550 Hz** | −110,5 dB (images) | −54,2 / −54,0 dB | 139 / 4 458 / 4 166 |
| 96 → 48 | 128 | **−1,002** | −99,1 / −17,7 dB | −146,8 dB | −83,0 dB | **−0,92 dB** | **18 950 Hz** | −157,1 dB (repliement) | −66,0 / −64,8 dB | 32 / 1 024 / 607 |
| 44,1 → 192 | 128 | −0,378 | −108,4 / −38,2 dB | −139,6 dB | −94,6 dB | **−10,31 dB** | **18 550 Hz** | −110,5 dB (images) | −52,5 / −52,6 dB | 278 / 8 916 / 8 333 |
| 176,4 → 48 (DSD64) | 256 | −0,171 | −99,4 / −33,0 dB | −143,1 dB | −99,2 dB | −0,03 dB | 20 400 Hz | −144,9 dB (repliement) | −74,0 / −74,5 dB | 34 / 557 / 448 |
| 192 → 44,1 | 512 | −0,201 | **−87,8** / −30,9 dB | −144,1 dB | −87,8 dB | −0,04 dB | 20 200 Hz | −145,1 dB (repliement) | −76,6 / −79,5 dB | 58 / 471 / 294 |

Sur les 7 rapports : longueur de piste = `round(n·vers/de)` exactement ;
gain à 1 kHz = 0 ± 0,0004 dB ; phase linéaire (délai à 10 kHz = délai à 1 kHz
à 1e−7 près) ; **blocs de 1 024 = blocs de 4 096 = piste d'un bloc, écart
0,0 exactement** ; le vidage rend les mêmes 64 dernières trames utiles que la
piste (erreur identique au bord de fin).

## Ce qui est prouvé

1. **Le gapless ne dépend pas de la taille des blocs** : le chemin du
   producteur (`rubato_resample_chunk` par blocs de 1 024 ou 4 096, puis
   `flush`) rend, une fois `output_delay()` retiré, les mêmes échantillons
   f32 que `rubato_resample_track`, au bit près, sur les 7 rapports.
2. **Le vidage est complet et déterministe** : `flush` traite le reste du
   bloc puis un bloc de silence ; le flux rend de 294 à 8 333 trames au-delà
   de `délai + attendu` (une queue de silence filtré, jamais une troncature).
3. **La réjection tient partout** : images −109,8 à −110,5 dB en montée,
   repliements −139,9 à −157,1 dB en descente — mieux que le seuil audiophile
   de 100 dB, y compris sur le noyau 128.
4. **Tune ne distord pas** : THD+N à 1 kHz entre −139,6 et −146,8 dB, au
   plancher du f32. L'erreur RMS contre la référence (−87,8 à −108,4 dB) est
   entièrement le **gain en bande passante** (ondulation de 3e−5 à 3,6e−4 dB),
   pas de la distorsion.
5. **La phase est linéaire** et le délai est expliqué à 0,001 trame près par
   `(sinc_len/2 − 1/256)·ratio − 1` (le 1/256 est le pas de la table
   suréchantillonnée de rubato).

## Ce qui n'est pas tenu (témoins `#[ignore]`, 15)

### D1 — −10,3 dB à 20 kHz pour toute source 44,1 kHz (5 rapports)

Le noyau 128 (`de/vers ≤ 2`) avec `calculate_cutoff(128, BlackmanHarris2)`
place la transition entre 18,5 et 22 kHz : **bande à −0,1 dB = 18 550 Hz,
−10,31 dB à 20 kHz** en 44,1 → 48, 44,1 → 96 et 44,1 → 192 ; **18 450 Hz et
−9,90 dB** en 48 → 44,1 ; 18 950 Hz et −0,92 dB en 96 → 48. Les deux méthodes
(impulsion, sinus) concordent à 1e−5 dB. Le commentaire de #2711 dans
`resample.rs` promet « within 0.1 dB » aux frontières de cadence — c'est vrai
**à 18 kHz**, seule fréquence que l'ancien banc mesure, et faux à 20 kHz. Les
noyaux 256 et 512 tiennent 20 kHz (−0,03 / −0,04 dB). Témoins :
`audiophile_bande_20k_{44_1_vers_48, 48_vers_44_1, 44_1_vers_96, 96_vers_48, 44_1_vers_192}`.

### D2 — délai résiduel jamais nul après retrait de `output_delay()` (7 rapports)

`rubato_resample_track` retire `output_delay()` = ⌊sinc_len/2·ratio⌋ trames,
mais le délai vrai du sinc de rubato vaut `(sinc_len/2 − 1/256)·ratio − 1`.
Reste **−0,17 à −1,00 trame** (Tune en avance) : en **96 → 48 kHz, une trame
entière** de musique est perdue en tête de chaque piste convertie (#1525) ou
décodée en bloc (#2246), et une trame de queue filtrée la remplace. Le chemin
en flux (`outputs/local.rs`) n'est pas concerné : il ne retire rien. Sans
alignement, l'erreur contre la référence est de −17,7 à −38,2 dB — c'est ce
que verrait un banc d'identité sample-exact. Témoins :
`r*::audiophile_delai_residuel_nul`.

### D3 — erreur en bande > −100 dB sur trois rapports (3 rapports)

96 → 48 : −99,1 dB ; 176,4 → 48 : −99,4 dB ; 192 → 44,1 : **−87,8 dB**
(gain +0,00036 dB à 1 kHz, noyau 512). Ce n'est pas de la distorsion (THD+N
< −143 dB) mais l'ondulation de bande passante du noyau. Le seuil de −100 dB
est celui d'un mot de 24 bits (plancher −144 dBFS) ; −87,8 dB correspond au
LSB d'un mot de 14,6 bits. Témoins :
`audiophile_erreur_1k_{96_vers_48, 176_4_vers_48, 192_vers_44_1}`.

### Ce qui n'est pas prouvé

* Les bords : −52 à −79 dB sur les 64 premières / dernières trames. Ce
  n'est pas un défaut établi — la référence sonne 512 trames avant un départ
  dur, Tune 64 à 256 — mais la valeur est surveillée (± 3 dB). Un banc de
  pré-écho exigerait un signal enveloppé, pas un départ dur.
* Le module à Nyquist utile en descente (−12 à −32 dB) est mesuré sur
  l'impulsion décimée, repliement compris : il n'est pas interprété.
* Le DSD lui-même (modulateur, filtre DSD → PCM) : T3 (`wvunpack`) ; ici
  seulement la cadence PCM qu'il produit.
* Le chemin en flux avec changement de cadence en cours de route
  (`set_resample_ratio`) : non exercé.

## Correctif du 13/09 — D1 est corrigé

Mesuré sur Shrek, `fix/src-bande-20khz-f70496` sur
`origin/batch/refonte-coeur-2` (`e818f2f7`), rubato 3.0.0.

### Ce qui était faux, et de quelle grandeur

Deux décisions se trompaient d'unité dans `new_streaming_resampler` :

1. **Le noyau était choisi sur le RAPPORT des cadences.** La largeur de
   transition d'un noyau sinc est fixée par sa **durée**, `sinc_len / from_sr`,
   donc elle vaut `K · from_sr / sinc_len` hertz — le rapport n'y entre pas. Et
   la bande utile disponible est bornée par la **plus basse** des deux cadences.
   Or 44,1 kHz est le cas le plus dur de tout l'audio : 20 kHz y occupent 90,7 %
   de Nyquist, il ne reste que 2 050 Hz pour toute la transition. Un rapport
   proche de 1 — 44,1 → 48 — tombait donc dans la branche « 128 », la plus
   courte, exactement là où il fallait la plus longue.
2. **La sortie locale ne suivait pas.** `outputs/local.rs` tenait **deux
   copies** de la table de paramètres, restées aux **32/64 coefficients**
   d'avant #2711 : le correctif de l'époque n'avait touché que
   `audio/resample.rs`. Le chemin du DAC rééchantillonnait donc deux fois plus
   court que le convertisseur de fichiers. Les deux copies sont supprimées ;
   `new_streaming_resampler` est le seul constructeur du dépôt.

### Le choix, et pourquoi

`FENETRE = Blackman2`, et la longueur retenue est **la première du barème
{128, 256, 512, 1024} dont la bande à −0,1 dB atteint 20 500 Hz** (20 kHz
promis + 500 Hz de marge), calculée par

```
bande(N) = calculate_cutoff(N, Blackman²) · min(from, to)/2  −  2,82 · from / N
```

Le premier terme est relatif à la cadence la plus **basse** (rubato met sa
coupure à l'échelle du rapport en descente) ; le second à la cadence
d'**entrée**, parce que la transition est fixée par la durée du noyau. La
constante 2,82 est relevée sur ce banc : elle prédit les sept rapports mesurés
**à moins de 40 Hz près**.

Sur les sept rapports du banc, le barème donne 256 partout sauf 192 → 44,1
(512). Le témoin `le_choix_du_noyau_suit_la_cadence_la_plus_basse` l'exerce sur
les **8 × 7 couples** de cadences que le produit annonce, et c'est lui qui a
trouvé un trou qu'aucun des sept rapports ne mesure : **352,8 et 384 → 44,1
kHz** — le PCM de DSD256 servi à une zone à la cadence du CD — ne rendent que
19 698 et 19 526 Hz même à 512 coefficients. Ce sont les couples les plus durs
du jeu : Nyquist bas à 22,05 kHz (2 050 Hz pour toute la transition) **et** un
noyau qui, à 352,8 kHz d'entrée, ne dure que 1,45 ms à 512 coefficients. D'où
le quatrième barreau, **1 024**, qui les porte à 20 873 et 20 786 Hz. Il ne
sert qu'à eux ; 128 ne suffit, lui, à **aucun** couple du produit.

### Les combinaisons essayées

Mesurées sur 44,1 → 48 (le couple contraignant), même instrumentation :

| configuration | gain 20 kHz | bande −0,1 dB | réjection |
|---|---|---|---|
| BH² N=128 (**avant**) | −10,31 dB | 18 550 Hz | −109,8 dB |
| BH² N=256 | −0,00 dB | 20 300 Hz | −115,8 dB |
| BH² N=384 | −0,00 dB | 20 900 Hz | −116,9 dB |
| BH² N=512 | −0,00 dB | 21 200 Hz | −109,6 dB |
| BH² N=256, `f_cutoff` = 0,970 | −0,00 dB | 20 800 Hz | −116,2 dB |
| BH N=128 | −0,31 dB | 19 850 Hz | −106,2 dB |
| BH N=256 | −0,00 dB | 20 950 Hz | −108,8 dB |
| Blackman² N=128 | −1,66 dB | 19 450 Hz | −108,5 dB |
| Blackman² N=192 | −0,00 dB | 20 350 Hz | −111,9 dB |
| **Blackman² N=256 (retenu)** | **−0,00 dB** | **20 750 Hz** | **−116,4 dB** |
| Blackman² N=384 | −0,00 dB | 21 200 Hz | −115,8 dB |
| Blackman² N=512 | 0,00 dB | 21 400 Hz | −109,5 dB |
| Blackman² N=256, `f_cutoff` = 0,975 | −0,00 dB | 21 050 Hz | −113,8 dB |
| Blackman² N=256, `oversampling` = 512 | 0,00 dB | 20 750 Hz | **−126,2 dB** |

Lecture : à longueur égale, **Blackman² bat Blackman-Harris² de 450 Hz de
bande utile pour la même réjection** (sa constante de coupure vaut 9,51 contre
13,75, pour un plancher de lobes qui reste sous −110 dB). Relever `f_cutoff`
à la main gagne encore 300 Hz mais rogne la réserve de bande d'arrêt sans
qu'aucun rapport n'en ait besoin : écarté. `oversampling_factor = 512` gagne
7 à 10 dB de réjection en montée pour **zéro** opération de plus — mais double
la table (512 Kio balayés à chaque échantillon), et Tune tourne aussi sur
Raspberry Pi ; la réjection est déjà au-delà des 100 dB visés, donc écarté
aussi. Il reste disponible si un besoin apparaît.

### Avant / après, les sept rapports

| rapport | noyau av. → ap. | gain 20 kHz av. → ap. | bande −0,1 dB av. → ap. | réjection av. → ap. |
|---|---|---|---|---|
| 44,1 → 48 | 128 → 256 | **−10,31 → −0,00 dB** | **18 550 → 20 750 Hz** | −109,8 → −116,4 dB |
| 48 → 44,1 | 128 → 256 | **−9,90 → −0,00 dB** | **18 450 → 20 700 Hz** | −139,9 → −122,6 dB |
| 44,1 → 96 | 128 → 256 | **−10,31 → −0,00 dB** | **18 550 → 20 750 Hz** | −110,5 → −111,1 dB |
| 96 → 48 | 128 → 256 | **−0,92 → +0,00 dB** | **18 950 → 22 050 Hz** | −157,1 → −134,1 dB |
| 44,1 → 192 | 128 → 256 | **−10,31 → −0,00 dB** | **18 550 → 20 750 Hz** | −110,5 → −111,1 dB |
| 176,4 → 48 (DSD64) | 256 → 256 | −0,03 → −0,00 dB | 20 400 → 21 150 Hz | −144,9 → −144,3 dB |
| 192 → 44,1 | 512 → 512 | −0,04 → −0,00 dB | 20 200 → 20 600 Hz | −145,1 → −145,8 dB |

Les deux derniers rapports gardent leur noyau : **seule la fenêtre change**, et
elle leur rend tout de même 750 et 400 Hz de bande.

La réjection baisse là où elle était très large (96 → 48 : −157 → −134 dB ;
48 → 44,1 : −140 → −123 dB) parce qu'une transition plus raide rapproche la
bande d'arrêt. Elle reste partout **au-delà de 111 dB**, soit 11 dB de marge
sur le seuil audiophile de 100 dB, et sous le plancher d'un mot de 24 bits
(−144 dBFS) sur les quatre rapports en descente.

### Erreur RMS et THD+N

| rapport | erreur RMS 1 kHz av. → ap. | THD+N av. → ap. |
|---|---|---|
| 44,1 → 48 | −108,4 → **−121,4 dB** | −140,2 → −136,8 dB |
| 48 → 44,1 | −105,0 → **−118,1 dB** | −140,1 → −138,2 dB |
| 44,1 → 96 | −108,4 → **−121,4 dB** | −139,8 → −136,1 dB |
| 96 → 48 | −99,1 → **−88,4 dB** | −146,8 → −146,3 dB |
| 44,1 → 192 | −108,4 → **−121,4 dB** | −139,6 → −135,9 dB |
| 176,4 → 48 | −99,4 → **−102,0 dB** | −143,1 → −144,5 dB |
| 192 → 44,1 | −87,8 → **−85,4 dB** | −144,1 → −144,6 dB |

Cinq rapports sur sept gagnent 13 dB. **Deux régressent** : 96 → 48 de 10,7 dB
et 192 → 44,1 de 2,4 dB. Il faut le dire précisément : dans les deux cas
l'écart est **entièrement un gain scalaire en bande** — +0,00033 dB (0,0038 %)
en 96 → 48, −0,00047 dB en 192 → 44,1 — et non de la distorsion : le THD+N est
inchangé, à −146,3 et −144,6 dB. C'est exactement le défaut **D3**, la
normalisation de gain de la fenêtre, qui n'est pas traité ici.

### Coût CPU et latence

5 minutes de stéréo, blocs de 1 024 trames, `--release` sur Shrek :

| chemin | avant | après | rapport | débit après |
|---|---|---|---|---|
| partagé, 44,1 → 48 (N=128 → 256) | 1,422 s | 2,150 s | **×1,51** | 140 × temps réel |
| sortie locale, 44,1 → 48 (N=64 → 256) | 1,030 s | 2,150 s | **×2,09** | 140 × temps réel |
| partagé, 96 → 48 (N=128 → 256) | 1,193 s | 1,768 s | **×1,48** | 170 × temps réel |
| sortie locale, 96 → 48 (N=64 → 256) | 0,904 s | 1,768 s | **×1,96** | 170 × temps réel |

Le chemin partagé reste **sous 2 ×**, la cible annoncée. La sortie locale paie
2,09 × parce qu'elle partait de 64 coefficients — un noyau qui ne tenait aucune
des promesses du produit. Dans l'absolu on reste à **140 × le temps réel** pour
un cœur : le rééchantillonnage occupe 0,7 % d'un cœur par zone.

Latence (délai de groupe, `output_delay()`) :

| chemin | avant | après |
|---|---|---|
| partagé, 44,1 → 48 | 69 trames · 1,44 ms | 139 trames · 2,90 ms |
| sortie locale, 44,1 → 48 | 34 trames · 0,71 ms | 139 trames · 2,90 ms |
| partagé, 96 → 48 | 32 trames · 0,67 ms | 64 trames · 1,33 ms |
| sortie locale, 96 → 48 | 16 trames · 0,33 ms | 64 trames · 1,33 ms |

Au pire **+2,2 ms**, une fois pour toute la chaîne sur le chemin en flux (le
rééchantillonneur y survit d'une piste à l'autre).

### Ce que le correctif change dans le rendu, et ce qu'il ne change pas

**Il change le son, volontairement.** Deux empreintes de R1
(`empreinte_du_puits_r1.rs`) sont remesurées :
`EMPREINTE_REECHANTILLONNAGE_44100_VERS_48000` (`0x4491…a9ee` →
`0x7d6d…4f8d`) et `EMPREINTE_ADAPTATION_PUIS_REECHANTILLONNAGE`
(`0x8c1c…9aaa` → `0x6cc3…25ed`). C'est le **filtre**, pas l'ordre des étages :
le compte de mots de la chaîne complète est inchangé (8 914), et il serait le
premier à sauter si l'ordre avait bougé. Ces deux témoins injectaient jusqu'ici
leur propre noyau de 64 coefficients, une valeur qui n'existait plus en
production — ils imageaient un filtre imaginaire ; ils prennent désormais
`new_streaming_resampler`.

**Il ne touche à rien d'autre**, et c'est vérifié plutôt qu'affirmé :

* `EMPREINTE_IDENTITE_16_BITS_STEREO` et
  `EMPREINTE_ADAPTATION_STEREO_VERS_MONO` **n'ont pas bougé d'un bit** : ni le
  chemin identité — celui de l'immense majorité des lectures — ni l'adaptation
  de canaux ne traversent le rééchantillonneur.
* Les trois empreintes de T8 (`capture_bout_en_bout_2218.rs`) sont
  **inchangées et n'ont pas été touchées** : ce banc monte sa chaîne en
  44,1 kHz vers une sortie 44,1 kHz, il ne rééchantillonne jamais.
* **Le bit-perfect et le DoP ne passent pas par le rééchantillonneur** :
  `versioned_dop_fixture_*`, `native_windows_ring_*` et
  `un_porteur_dop_refuse_n_ecrit_rien_dans_le_puits` passent sans retouche. Le
  porteur DoP est même refusé *avant* l'écriture (#3233) — il ne survivrait ni
  au sinc ni à l'adaptation de canaux.

**Ce correctif doit être écouté avant publication** (Mac et .42) : c'est le
premier changement de rendu délibéré du chemin de lecture.

### Témoins dé-ignorés

Les 5 témoins D1 passent au vert et sont exécutés :
`audiophile_bande_20k_{44_1_vers_48, 48_vers_44_1, 44_1_vers_96, 96_vers_48,
44_1_vers_192}`. `audiophile_erreur_1k_176_4_vers_48` passe aussi
(−99,4 → −102,0 dB) : 6 en tout. Il reste **9 ignorés** : D2 (7) et D3 (2,
avec leurs motifs remis à jour).

### Ce qui reste, et n'est pas traité ici

* **D2** — `rubato_resample_track` retire `⌊sinc_len/2·ratio⌋` trames alors
  que le délai vrai vaut `(sinc_len/2 − 1/256)·ratio − 1` : il reste −0,17 à
  −1,00 trame de décalage, une trame entière perdue en tête en 96 → 48.
  Inchangé par ce correctif (le délai résiduel bouge avec le noyau, le défaut
  non) — **et vérifié tel quel après lui** : voir
  « [Correctif du 13/09 — D2](#correctif-du-1309--d2-est-corrigé) », #4078.
* **D3** — l'erreur en bande reste au-dessus de −100 dB sur 96 → 48
  (−88,4 dB) et 192 → 44,1 (−85,4 dB), et ce correctif l'a **aggravée** sur ces
  deux-là. C'est un gain scalaire, pas de la distorsion. Piste inchangée :
  `oversampling_factor`, ou une normalisation explicite du gain continu du
  noyau.


## Correctif du 13/09 — D2 est corrigé

Mesuré sur Shrek, `fix/4078-delai-residuel-src-b209` sur `origin/batch/bugs-13`
(`baa7f19d`), rubato 3.0.0 — donc **sur le noyau d'après D1** (Blackman²,
barème 256/512/1024).

Première question, posée avant d'écrire une ligne : **D2 survit-il au correctif
de D1 ?** Oui, à l'identique. Les sept témoins `audiophile_delai_residuel_nul`
relevés sur `batch/bugs-13` **avant toute retouche** rendent −0,404 / −0,685 /
−0,369 / −1,002 / −0,738 / −0,171 / −0,201 trame : les chiffres du relevé du
12/09 sont inchangés par D1, et « une trame entière perdue en 96 → 48 kHz »
tient toujours.

### Le délai vrai, démontré au lieu d'être relevé

La formule `(sinc_len/2 − 1/256)·ratio − 1` était une constatation du banc.
Elle se lit maintenant dans le code de rubato 3.0.0 — et c'est cette lecture
qui dit ce qui est réductible et ce qui ne l'est pas :

* `InnerSinc::init_last_index()` (`asynchro_sinc.rs`) vaut `−(N − 1)`, et
  `process` (`asynchro.rs`) avance `idx` de `1/ratio` **avant** chaque trame :
  la trame de sortie `n` est prise à `idx_n = −(N−1) + (n+1)/ratio` ;
* `make_sincs` (`sinc.rs`) range la phase `s` de sorte que son lobe central
  tombe en `N/2 − 1 + (s+1)/F` de la fenêtre (`F = oversampling_factor = 256`),
  alors que `get_nearest_times_2` (`interpolation.rs`) rend
  `s = ⌊frac(idx)·F⌋` : **la table couvre `(0, 1]` là où l'index couvre
  `[0, 1)`**. La position réellement évaluée vaut donc `idx + N/2 − 1 + 1/F`.

En recollant les deux bouts : la trame de sortie `n` échantillonne l'entrée à
l'instant `(n+1)/ratio − N/2 + 1/F`. Le délai vaut donc `N/2 − 1/F − 1/ratio`
trames d'ENTRÉE, soit **`(N/2 − 1/F)·ratio − 1` trames de sortie**.
`output_delay()`, lui, rend `⌊N/2 · ratio⌋` : un arrondi **par défaut** d'une
valeur voisine, jamais la valeur.

### La fraction : ce qu'on peut en faire, et ce qu'on ne peut pas

Depuis l'extérieur de rubato, la grille de sortie ne bouge pas : ses instants
sont `m/ratio + r`, `r` constant. Il n'existe que deux leviers, tous deux
**entiers** — un pré-roll de `p` trames d'entrée, et le retrait de `k` trames
de sortie. L'écart restant vaut

```
avance(p, k) = (k+1)/ratio − (N/2 − 1/F) − p      trames d'entrée
```

et avec `ratio = L/M` réduit, `(k+1)/ratio` ne prend que des multiples de
`1/L`. Le terme `1/F = 1/256` n'est un multiple de `1/L` que si `256 | L` —
**jamais pour les cadences du produit**. Le résidu est donc *irréductible*,
borné par `min(1/F, 1/(2L))` trame d'entrée.

**Le choix : arrondi assumé, résidu chiffré.** `alignement_de_piste` balaie `p`
sur une période (`M = de/pgcd(de, vers)`), prend `k = round(délai vrai)` et
garde le couple qui minimise `|résidu|`. Sans pré-roll, l'arrondi seul
laisserait **−0,40 à +0,32 trame** sur les sept rapports — le contrat
« exact » ne tiendrait pas. Avec, il reste au pire **1/256 de trame
d'ENTRÉE**, et le témoin `alignement_residu_borne_sur_toutes_les_cadences`
le verrouille sur les **8 × 7 couples** que le produit annonce : pire relevé
**0,003906 trame d'entrée**, soit exactement `1/256`. À 44,1 kHz : **89 ns**.

Le pré-roll coûte `p < M` trames de zéros en tête — au pire 155 trames, 3,5 ms
à 44,1 kHz — jetées avec le délai.

### Les sept rapports

| rapport | résidu av. → ap. | pré-roll | retrait (`output_delay()` d'avant) | identité SANS recalage av. → ap. |
|---|---|---|---|---|
| 44,1 → 48 | **−0,685 → +0,00255** | 53 | 196 (139) | −21,0 → **−69,5 dB** |
| 48 → 44,1 | **−0,404 → +0,00266** | 155 | 259 (117) | −24,8 → **−68,4 dB** |
| 44,1 → 96 | **−0,369 → −0,00170** | 36 | 356 (278) | −32,3 → **−79,1 dB** |
| 96 → 48 | **−1,002 → −0,00195** | 0 | 63 (64) | −17,7 → **−71,7 dB** |
| 44,1 → 192 | **−0,738 → −0,00340** | 36 | 713 (557) | −32,3 → **−79,1 dB** |
| 176,4 → 48 | **−0,171 → −0,00106** | 19 | 39 (34) | −33,0 → **−77,1 dB** |
| 192 → 44,1 | **−0,201 → +0,00067** | 27 | 64 (58) | −30,9 → **−79,3 dB** |

La dernière colonne est ce que voit un banc d'identité sample-exact : la
référence à délai NUL, sans recalage. Le plancher qui reste — −68 à −79 dB —
est le résidu de délai lui-même et rien d'autre ; la mesure RECALÉE, elle, ne
bouge pas (−121,4 / −118,2 / −121,3 / −88,4 / −121,4 / −102,1 / −85,4 dB).

Le délai mesuré par la phase à 1 kHz colle à ce que la production annonce
(`alignement_de_piste`) à **1e−8 près**, et le délai à 10 kHz au délai à 1 kHz
à 1e−6 près : la phase reste linéaire.

### Ce que le correctif change dans le rendu, et ce qu'il ne change pas

**Il change le son, et c'est le but.** En 96 → 48 kHz, une trame entière de
musique cessait d'exister en tête de chaque piste convertie (#1525) ou décodée
en bloc (#2246), remplacée par une trame de queue filtrée : elle revient. Sur
les six autres rapports, c'est une fraction de trame de décalage qui disparaît.
La v0.9.148 venait de changer le rendu du rééchantillonneur (D1) ; **celui-ci
est le second changement délibéré, et il doit être écouté avant publication.**

Ce qui ne bouge pas, vérifié plutôt qu'affirmé : `err_sinus_db`,
`err_balayage_db`, `gain_20k_db`, `bande_hz`, `thd_n_db`, `delai_annonce`,
`vidage_trames`, `marge_queue` — identiques à 0,1 dB / 0 trame près sur les
sept rapports. Les empreintes de R1 (`empreinte_du_puits_r1.rs`) ne bougent
**pas d'un bit** : elles passent par `new_streaming_resampler` et
`rubato_resample_chunk`, pas par la piste. L'identité blocs de 1 024 / 4 096 /
piste reste **exacte à 0** — recadrée du même pré-roll, la piste n'est toujours
QUE le flux.

**Une valeur bouge, et c'est un effet de MESURE** : la réjection des images des
trois montées depuis 44,1 kHz, la seule qui se mesure sur une IMPULSION. Son
plancher est l'interpolation linéaire de rubato entre phases — une erreur qui
n'est pas à bande limitée, donc qui dépend de l'endroit où l'impulsion tombe
par rapport à la grille de sortie. Aligner la piste l'y ramène. Balayé sur
44,1 → 48 en faisant varier le pré-roll :

| pré-roll | résidu | réjection |
|---|---|---|
| 19 | −0,00425 | −107,4 dB |
| **53 (retenu)** | **+0,00255** | **−109,9 dB** |
| 87 | +0,00935 | −116,6 dB |

Le troisième garde le chiffre d'avant et passerait le seuil de 0,01 trame : il
est **refusé**, ce serait choisir un décalage 3,7 fois plus grand pour flatter
une mesure. Après correctif : −109,9 dB (44,1 → 48), −107,4 (44,1 → 96),
−107,5 (44,1 → 192). Les quatre autres rapports mesurent leur réjection sur un
TON et ne bougent pas (−122,6 / −134,1 / −144,0 / −145,5 dB). Partout au-delà
de **107 dB**, soit 7 dB de marge sur le seuil audiophile de 100 dB.

### Témoins dé-ignorés

Les 7 témoins `r*::audiophile_delai_residuel_nul` passent au vert et sont
exécutés. Il reste **2 ignorés**, tous D3 :
`audiophile_erreur_1k_{96_vers_48, 192_vers_44_1}`.

Deux témoins gagnent en plus une affirmation :
`delai_residuel_affirme_la_mesure` affirme désormais le couple
`(pré-roll, retrait)` que la PRODUCTION décide — et non une copie du calcul,
qui pourrait diverger en silence comme le barème des noyaux l'avait fait avant
D1 ; `erreur_rms_sinus_1k_affirme_la_mesure` affirme l'identité SANS recalage,
là où il vérifiait auparavant qu'elle était mauvaise.

### Ce qui reste, et n'est pas traité ici

* **`StreamingPcmAdapter` (`audio/decode.rs`) porte le MÊME défaut**, et il
  n'est pas corrigé ici. Il retire lui aussi `output_delay()`
  (`resampler_delay_remaining`), pour les décodages progressifs servis en
  HTTP ; il crée son rééchantillonneur par piste, donc `alignement_de_piste`
  s'y appliquerait tel quel. Aucun témoin ne le mesure aujourd'hui : le
  corriger sans banc serait un changement de rendu non prouvé sur le chemin
  servi aux testeurs. À ouvrir séparément, avec son banc.
* **D3** — inchangé : l'erreur en bande reste au-dessus de −100 dB sur
  96 → 48 (−88,4 dB) et 192 → 44,1 (−85,4 dB). C'est un gain scalaire, pas de
  la distorsion.
* **Le résidu de 1/256 de trame d'entrée** est irréductible sans toucher à
  rubato. Le supprimer demanderait soit un filtre de retard fractionnaire de
  plus dans la chaîne — un second FIR, pour 89 ns — soit un correctif chez
  rubato sur l'indexation `(0, 1]` / `[0, 1)` de sa table. Ni l'un ni l'autre
  ne se justifie à cette échelle.


## Contre-épreuves

Altération 1 — la valeur attendue de bande passante du premier rapport
(`cp` avant, `sed`, `touch`) :

```
394c394
<         bande_hz: 18_550.0,
---
>         bande_hz: 20_000.0,
=== ROUGE
EXIT=101
test result: FAILED. 66 passed; 1 failed; 15 ignored; 0 measured; 0 filtered out; finished in 17.50s
44,1 → 48 kHz : bande passante à −0,1 dB = 18550 Hz, attendu 20000 ± 100 (pas de balayage 50 Hz) ; ondulation crête 20 Hz → 20 kHz 10.311 dB, module à Nyquist utile -109.7 dB
=== VERT (cp, touch)
EXIT=0
test result: ok. 67 passed; 0 failed; 15 ignored; 0 measured; 0 filtered out; finished in 19.70s
5d61a10e80791f4964037e72a46a655b  tune-core/tests/reechantillonnage_reference_2218.rs
5d61a10e80791f4964037e72a46a655b  /tmp/reechantillonnage-avant-nuitG-f70496.rs
```

Altération 2 — le rapport DSD64 (176,4 → 88,2 kHz de source) :

```
346c346
<         de: 176_400,
---
>         de: 88_200,
=== ROUGE 2
EXIT=101
test result: FAILED. 60 passed; 7 failed; 15 ignored; 0 measured; 0 filtered out; finished in 21.48s
test audiophile_bande_20k_176_4_vers_48 ... FAILED
test r176_4_vers_48_dsd64::bande_passante_affirme_la_mesure ... FAILED
test r176_4_vers_48_dsd64::erreur_rms_sinus_1k_affirme_la_mesure ... FAILED
test r176_4_vers_48_dsd64::bords_affirment_la_mesure ... FAILED
test r176_4_vers_48_dsd64::longueur_et_vidage_affirment_la_mesure ... FAILED
test r176_4_vers_48_dsd64::erreur_rms_balayage_affirme_la_mesure ... FAILED
test r176_4_vers_48_dsd64::delai_residuel_affirme_la_mesure ... FAILED
176,4 → 48 kHz (PCM de DSD64) : gain à 20 kHz = -0.74 dB, attendu -0.03 ± 0,05
176,4 → 48 kHz (PCM de DSD64) : le vidage rend 1115 trames, attendu 557 (déterministe : reste du bloc de 1 024 puis un bloc de silence)
176,4 → 48 kHz (PCM de DSD64) : délai résiduel -0.1722 ≠ ((sinc_len/2 − 1/256)·ratio − 1) − output_delay() = 34.6577
=== VERT 2 (cp, touch)
EXIT=0
test result: ok. 67 passed; 0 failed; 15 ignored; 0 measured; 0 filtered out; finished in 21.37s
5d61a10e80791f4964037e72a46a655b  tune-core/tests/reechantillonnage_reference_2218.rs
```

Au passage, la seconde altération montre qu'un noyau 128 sur 88,2 → 48
(rapport 1,84) rend −0,74 dB à 20 kHz : le défaut D1 couvre bien toute la
branche `de/vers ≤ 2`, pas seulement la source 44,1 kHz.

## Témoins

82 dans la cible. Au 12/09 : **67 passés, 15 ignorés** (D1 : 5, D2 : 7,
D3 : 3). Après les correctifs D1 et D2 du 13/09 : **80 passés, 2 ignorés**
(D3 seulement), 0 rouge. Par rapport : `delai_residuel`, `erreur_rms_sinus_1k`,
`erreur_rms_balayage`, `bande_passante`, `rejection`, `bords`,
`longueur_et_vidage`, `blocs_de_1024_et_4096_rendent_la_piste_a_l_identique`
(×7), les seuils audiophiles `audiophile_bande_20k_*` (7, 5 ignorés),
`audiophile_erreur_1k_*` (7, 3 ignorés au 12/09, 2 aujourd'hui),
`audiophile_delai_residuel_nul` (×7, tous ignorés au 12/09, tous exécutés
aujourd'hui), les 4 preuves de la référence, et le relevé
(`releve_de_tous_les_rapports`, imprime tout avec `--nocapture`).

## Cases de #2218 (« SRC, remix et DSP »)

* « Nombre exact de frames pour chaque ratio et flush » — déjà cochée ;
  confirmée ici contre une référence (longueur exacte, vidage déterministe),
  **et la réserve de position est levée** depuis #4078 : exact en nombre ET
  en position, à 1/256 de trame d'entrée près.
* « Impulsion, sweep, bande passante, réjection d'alias, bruit et THD+N » —
  déjà cochée ; **ne devrait pas l'être** telle quelle : la bande passante
  n'avait jamais été mesurée à 20 kHz. Ce document la rend cochable **pour les
  noyaux 256 et 512** ; pour le noyau 128, elle reste ouverte jusqu'à D1.
* « Méthodes de mesure alignées avec AES17 » — inchangée : ici la référence
  est un sinc de Kaiser, pas un filtre AES17.

## Issues proposées

1. **« SRC : −10,3 dB à 20 kHz pour toute source 44,1 kHz (noyau 128, bande
   −0,1 dB = 18,55 kHz) »** — `new_streaming_resampler` choisit 128
   coefficients quand `de/vers ≤ 2` ; avec `calculate_cutoff(128,
   BlackmanHarris2)` la transition commence à 18,5 kHz. Mesuré contre une
   référence sinc indépendante (T10) : −10,31 dB à 20 kHz en 44,1 → 48/96/192,
   −9,90 dB en 48 → 44,1, −0,92 dB en 96 → 48. Le test existant balaie jusqu'à
   18 kHz. Piste : 256 coefficients sur cette branche, ou relever `f_cutoff`
   en vérifiant que la réjection reste > 100 dB (aujourd'hui −110 dB en
   montée). Dé-ignorer `audiophile_bande_20k_*`.
2. **« `rubato_resample_track` retire une trame de trop : délai résiduel
   −0,17 à −1,00 trame, une trame de musique perdue en 96 → 48 »** — le
   délai vrai vaut `(sinc_len/2 − 1/256)·ratio − 1`, `output_delay()` rend
   ⌊sinc_len/2·ratio⌋. Convertisseur (#1525) et pistes décodées en bloc
   (#2246) sont touchés, pas le flux. Piste : retirer `output_delay() − 1`
   trames, ou compenser la fraction par un rééchantillonnage à phase
   ajustée. Dé-ignorer `r*::audiophile_delai_residuel_nul`.
   → ouverte en **#4078**, **corrigée le 13/09** : le délai vrai est retiré,
   pré-roll d'entrée compris ; résidu ≤ 1/256 de trame d'entrée.
3. **« SRC 192 → 44,1 : erreur en bande −87,8 dB (gain +0,00036 dB), sous le
   seuil de 24 bits »** — noyau 512, ondulation de bande passante ; 96 → 48
   et 176,4 → 48 à −99 dB. Pas de distorsion (THD+N < −143 dB). Piste :
   `oversampling_factor` ou fenêtre. Dé-ignorer `audiophile_erreur_1k_*`.

## Reproduction

```sh
cargo test -p tune-core --test reechantillonnage_reference_2218
cargo test -p tune-core --test reechantillonnage_reference_2218 i4079         # gain continu
cargo test -p tune-core --test reechantillonnage_reference_2218 releve -- --nocapture
```

## Correctif du 15/09 — D3 : normalisation compensée (#4079)

JP Robbe / OpenAI Codex / jp-robbe-20260915-201930-4079.
Base serveur `24123a4e6b5589e7ce90bb1f3cbe7d132cc4d52e`. Mesures sur Shrek,
Rust 1.98.0, six jobs, target propre à cette unité.

### Cause et changement

`rubato 3.0.0/src/sinc.rs::make_sincs` additionne les coefficients du noyau
suréchantillonné dans le type de l'audio, ici `f32`, puis divise toute la table
par cette somme. Les petites queues du sinc perdent de la précision lorsque
la somme devient grande. L'erreur de normalisation se retrouve dans tous les
coefficients : c'est le décalage de gain dominant à 1 kHz.

Le correctif applique une sommation compensée de Kahan à cette seule somme.
Les valeurs brutes des coefficients, la fenêtre Blackman², le nombre de
coefficients, les 256 phases, l'interpolation linéaire et le calcul de délai
restent identiques. Aucun coût arithmétique n'est ajouté dans la boucle audio ;
la construction effectue trois opérations supplémentaires par coefficient.

La copie `vendor/rubato` reste en version 3.0.0. Elle diffère de l'archive
crates.io dans **un seul fichier source, `src/sinc.rs`** ; son origine, son
SHA-256 et sa licence sont conservés dans `TUNE-PATCH.md`. La dépendance est
un chemin interne, comme `rust_cast`, pour que les consommateurs externes de
`tune-core` reçoivent aussi le correctif. La dernière version publiée 5.0.0,
vérifiée le 15/09, conserve l'addition naïve.

### Résultats contre la même référence

| rapport | RMS 1 kHz avant / après (dB) | RMS balayage avant / après (dB) | THD+N après (dB) |
|---|---:|---:|---:|
| 44,1 → 48 kHz | -121.4 / **-135.7** | -108.4 / -107.2 | -136.8 |
| 48 → 44,1 kHz | -118.2 / **-137.7** | -110.8 / -108.7 | -137.9 |
| 44,1 → 96 kHz | -121.3 / **-135.4** | -108.4 / -107.1 | -136.3 |
| 96 → 48 kHz | -88.4 / **-137.2** | -88.4 / -118.4 | -144.6 |
| 44,1 → 192 kHz | -121.4 / **-135.3** | -108.4 / -107.2 | -136.3 |
| 176,4 → 48 kHz (PCM de DSD64) | -102.1 / **-143.4** | -102.3 / -130.0 | -144.6 |
| 192 → 44,1 kHz | -85.4 / **-144.5** | -85.4 / -132.0 | -144.6 |

Les sept rapports passent le seuil inchangé de −100 dB à 1 kHz. La bande
à −0,1 dB, la réjection, les bords, le délai, le nombre de trames et l'identité
blocs 1 024 / 4 096 / piste restent dans leurs gardes antérieures.
Le balayage se déplace de 1,2 à 2,1 dB vers une erreur un peu plus grande sur
les trois montées depuis 44,1 et sur 48 → 44,1 ; il reste sous −107 dB.
La suppression du biais global ne supprime pas l'erreur d'interpolation
entre phases. Ces déplacements sont consignés, pas effacés par une tolérance
plus large.

Seuls les relevés attendus RMS (sinus aligné, sinus non recalé, balayage)
ont été remesurés, avec leur tolérance précédente de ±1 dB. La référence,
les seuils audiophiles et les autres tolérances n'ont pas changé.
Le test de gain continu utilise une entrée constante et les trames centrales
après transitoires : erreur relative maximale **4,77e−7**, seuil **1e−6**,
sur les sept rapports. Il n'ajuste ni amplitude ni phase.

### Coût sur Shrek

Microbanc Rubato en release (`opt-level=2`, LTO thin, quatre unités de code),
stéréo, blocs de 1 024 trames, 300 secondes d'audio par rapport. Médiane de
trois passages alternés avant/après ; chaque mesure de construction regroupe
30 créations. Les paramètres correspondent à ceux de Tune : 256 coefficients
pour les trois premiers couples ci-dessous, 512 pour 192 → 44,1.

| rapport | construction avant / après (ms) | flux avant / après (s) | ratio flux |
|---|---:|---:|---:|
| 44,1 → 48 | 5.849 / 6.509 | 2.071 / 2.087 | 1.008 |
| 96 → 48 | 5.700 / 5.792 | 1.749 / 1.742 | 0.996 |
| 176,4 → 48 | 5.604 / 5.727 | 1.960 / 1.975 | 1.008 |
| 192 → 44,1 | 11.398 / 11.745 | 3.390 / 3.389 | 1.000 |

La boucle en flux reste entre 0,996× et 1,008× dans ces médianes.
La construction coûte jusqu'à environ 0,66 ms de plus dans ce relevé.
Shrek est partagé : la variabilité visible entre passages empêche d'en faire
une mesure fine de performance. Ce microbanc ne mesure ni le serveur entier,
ni un Raspberry Pi, ni une sortie matérielle.

Recette et résultats conservés sur Shrek :
`/tmp/jp-robbe-20260915-201930-4079-experiments/bench-{baseline,kahan}`,
`/tmp/jp-4079-bench-final-{baseline,kahan}-{1,2,3}.log`.
Le premier essai de microbanc employait par erreur 512 coefficients pour
176,4 → 48 ; il est exclu de ce tableau. Les fichiers `bench-final-*`
correspondent au noyau de production de 256 coefficients.

### Portée

Les tests comparent le PCM produit par les API publiques de Tune à une
référence indépendante. Ils ne prouvent pas le rendu d'un DAC, la performance
ARM ou la latence système. L'arrondi de phase résiduel décrit dans D2 et les
erreurs de bord restent mesurés ; ce correctif ne prétend pas les supprimer.


### Contre-épreuve exécutée

Les tests finaux sont conservés. Seul le fichier de production
`vendor/rubato/src/sinc.rs` est remplacé par celui de l'archive 3.0.0.

- Filtre `audiophile_erreur_1k` : compilation réussie, **2 rouges / 5 verts**,
  sur 96 → 48 et 192 → 44,1, avec les erreurs initiales −88,4 et −85,4 dB.
- Filtre `i4079` : **1 rouge**, gain continu relatif à 44,1 → 48 hors de
  1 ± 1e−6 (erreur maximale 1,192e−6).
- Restauration par `cp` du correctif puis banc complet : **83/83 verts**,
  aucun test ignoré.

Journaux Shrek : `/tmp/jp-4079-counterproof-d3.log`,
`/tmp/jp-4079-counterproof-dc.log` et `/tmp/jp-4079-t10-final.log`.

### Empreintes de sortie locale

Après mesure indépendante, les deux empreintes contenant le rééchantillonnage
sont remesurées dans `empreinte_du_puits_r1.rs`. Avant remesure : **270 verts /
2 rouges** sur la suite locale ; seuls ces deux témoins changent de mots.
Le compte de **8 914 mots** de la chaîne adaptation puis rééchantillonnage
reste identique ; les empreintes sans rééchantillonnage passent déjà.

- Rééchantillonnage seul : `0x7d6d2c8f4cee4f8d` → `0xf7abd26d0d563951`.
- Adaptation puis rééchantillonnage : `0x6cc3fd32396525ed` → `0xfff7fe9fcf7484c6`.

Le changement vient du gain de normalisation mesuré ci-dessus, pas d'une
réorganisation des étages ou d'un nouveau noyau.


Validation finale locale : **272/272** tests de sortie locale, **20/20**
tests unitaires du rééchantillonneur, Clippy correctness et formatage réussis.
La résolution Cargo depuis un petit consommateur extérieur au workspace
sélectionne bien la copie Rubato de ce dépôt (métadonnées vérifiées ;
ce contrôle ne vaut pas compilation séparée du consommateur).

## Garde du noyau 1 024 (#4080) — 16/09

Bertrand / Claude Code / claude-ab67-20260916 / b14-4080. Base
`origin/batch/bugs-14` à `3a2b710a`. Mesures sur Shrek.

Le correctif #4027 a ajouté le barreau **1 024** au barème pour deux couples
seulement, 352,8 et 384 → 44,1 kHz (le PCM de DSD256, ou du 384 kHz, servi à
une zone à la cadence du CD), qui rendaient 19 698 et 19 526 Hz à 512. Cette
section répond aux deux points ouverts par #4080 : la garde tourne-t-elle
vraiment en CI, et que coûte ce barreau.

### La garde des 8 × 7 couples tourne en CI — preuve par le journal

La garde est `le_choix_du_noyau_suit_la_cadence_la_plus_basse`, un test
**unitaire** de `tune-core/src/audio/resample.rs` (module `#[cfg(test)] mod
tests`, aucune `feature` requise ; `audio::resample` est déclaré sans `cfg`
dans `tune-core/src/audio/mod.rs`). Le banc T10 est la cible `[[test]]`
`reechantillonnage_reference_2218` de `tune-core/Cargo.toml`, **sans
`required-features`** — indispensable puisque `tune-core` porte
`autotests = false`.

Les deux sont exécutés par le job `Test` de `ci.yml` — job `test`, condition
`needs.impact.outputs.rust == 'true'` seulement, **pas** `full` — dont la ligne
est `cargo test --no-fail-fast -p tune-core … --no-default-features --features
oaat,cloud-relay,bandcamp`. Ni la garde ni le banc ne dépendent de
`local-audio`, absente de cette ligne.

Preuve exécutée, pas déduite : run CI **35094052996** (PR
`feat/4201-upnp-sync-codex`, 16/09/2026), job `Test` (**104787011751**), journal
relu ligne à ligne :

| ligne du journal | ce qu'elle dit |
|---|---|
| 1537 | `test audio::resample::tests::le_choix_du_noyau_suit_la_cadence_la_plus_basse ... ok` |
| 5402 | unitaires de `tune-core` : `4553 passed; 0 failed; 4 ignored` |
| 5750 | `Running tests/reechantillonnage_reference_2218.rs` |
| 5837 | banc T10 : `83 passed; 0 failed; 0 ignored`, 25,9 s |

Deux gardes de `tune-server/tests/workflows_bornes.rs` empêchent déjà que
`-p tune-core` disparaisse de cette ligne
(`tout_membre_du_workspace_est_execute_par_une_porte_cargo_test`, second
verdict) ; `tune-core/tests/tests_orphelins.rs` empêche qu'un fichier de
`tests/` perde sa cible `[[test]]`.

### Deux témoins MESURÉS de plus (85/85)

La garde unitaire exerce les 56 couples contre le **modèle**
`bande_a_moins_0_1_db` (coupure × Nyquist bas − 2,82 · from / N), pas contre le
filtre : quelqu'un qui retoucherait le barème **et** la constante 2,82 la
laisserait verte. Aucun des sept rapports du banc ne mesurait ces deux couples.
Deux témoins s'ajoutent, `audiophile_bande_20k_352_8_vers_44_1` et
`audiophile_bande_20k_384_vers_44_1`, avec la même instrumentation que les sept
rapports (impulsion → bande à −0,1 dB, sinus 20 kHz → gain) mais **sans la
référence sinc** : ses 1 025 coefficients sont posés à la cadence d'entrée, et
à 352,8 kHz sa transition (≈ 3 kHz) dépasserait les 2 050 Hz qui séparent
20 kHz de Nyquist bas. Une bande et un gain ne demandent pas de référence.

| rapport | noyau | bande −0,1 dB mesurée | modèle | gain 20 kHz | ondulation |
|---|---:|---:|---:|---:|---:|
| 352,8 → 44,1 kHz (PCM de DSD256) | 1 024 | **20 900 Hz** | 20 873 Hz | 0,000 dB | 0,0000 dB |
| 384 → 44,1 kHz | 1 024 | **20 800 Hz** | 20 786 Hz | 0,000 dB | 0,0001 dB |

Le modèle prédit la mesure à moins de 30 Hz, comme sur les sept autres. Les
deux témoins tiennent en 1,8 s (debug) ; le banc passe de 83 à **85** tests.

### Contre-épreuve

Barreau précédent forcé pour 384 → 44,1 kHz dans `parametres_sinc` (deux
lignes temporaires, `sinc_len = 512` pour ce seul couple), rsync + touch,
puis :

- `cargo test -p tune-core --no-default-features --features oaat,cloud-relay --lib -- le_choix_du_noyau_suit_la_cadence_la_plus_basse`
  → **FAILED**, `resample.rs:647` :
  `384000 → 44100 : noyau 512, bande à −0,1 dB = 19526 Hz — Tune promet 20 kHz`
- `cargo test -p tune-core … --test reechantillonnage_reference_2218 -- audiophile_bande_20k_352_8_vers_44_1 audiophile_bande_20k_384_vers_44_1`
  → **1 passed, 1 failed** : `384 → 44,1 kHz : noyau 512 coefficients, bande à
  −0,1 dB MESURÉE = 19550 Hz, gain à 20 kHz = -0.39 dB ; Tune promet 20 kHz à
  −0,1 dB …` — et 352,8 → 44,1, non saboté, reste vert : le témoin nomme le
  bon couple.

Restauration par `cp` depuis la sauvegarde, rsync + touch : vert.

### Coût du barreau 1 024 — sur Shrek, x86_64, PAS sur Raspberry Pi

**Machine** : Shrek, Intel Xeon E5-2630 v4 @ 2,20 GHz (Broadwell, 10 cœurs /
20 fils par socket, 40 fils vus), **en régime partagé** : charge moyenne 80 à
90 pendant les mesures, fréquence à 79 % du maximum. Un seul fil par mesure.
**Ce n'est pas un Raspberry Pi** : le coût absolu ci-dessous ne s'y transpose
pas ; seuls les **rapports** entre lignes (même boucle, même machine, même
minute) ont un sens portable.

**Protocole** (même que « Coût CPU et latence » du 13/09) : 5 minutes de
stéréo, blocs de 1 024 trames, chemin du producteur `rubato_resample_chunk`
puis `flush`, binaire `--release` (`opt-level=2`, LTO thin), génération du
signal **exclue** du chronomètre (une seconde de sinus pré-calculée et
rejouée), `/usr/bin/time` sur le processus entier. « prod » = le noyau que
`parametres_sinc` choisit ; « 512 forcé » = mêmes paramètres (Blackman²,
256 phases, interpolation linéaire) avec `sinc_len = 512`. Deux passages ;
les deux sont donnés, l'écart entre eux (≤ 6 %) est le bruit de Shrek.

| rapport | noyau | passage 1 | passage 2 | × temps réel | % d'un cœur | RSS max |
|---|---:|---:|---:|---:|---:|---:|
| 352,8 → 44,1 | **1 024 (prod)** | 7,66 s | 8,17 s | 37–39 × | **2,6–2,7 %** | 6,3 Mo |
| 352,8 → 44,1 | 512 forcé | 4,76 s | 4,94 s | 61–63 × | 1,6–1,7 % | 5,8 Mo |
| 384 → 44,1 | **1 024 (prod)** | 9,75 s | 9,47 s | 31–32 × | **3,2–3,3 %** | 6,6 Mo |
| 384 → 44,1 | 512 forcé | 5,76 s | 5,74 s | 52 × | 1,9 % | 5,8 Mo |
| 44,1 → 48 | 256 (prod) | 3,87 s | 3,74 s | 78–80 × | 1,2–1,3 % | 3,2 Mo |
| 192 → 44,1 | 512 (prod) | 5,67 s | 5,70 s | 53 × | 1,9 % | 4,5 Mo |

`user` = `elapsed` à 0,05 s près, `sys` = 0 : c'est du calcul pur, un fil.

Lecture :

* Le barreau 1 024 coûte **× 1,61 à 1,69** par rapport à 512 sur les deux
  couples qu'il sert (7,9 s contre 4,9 s ; 9,6 s contre 5,7 s pour 5 minutes).
  Pas × 2, parce qu'une part du coût (lecture des 352 800 trames d'entrée par
  seconde, blocs, copies) ne dépend pas de la longueur du noyau.
* Dans l'absolu, sur ce Xeon : **2,6 à 3,3 % d'un cœur par zone**, 31 à 39 ×
  le temps réel. C'est le couple le plus cher de tout le barème — 2,5 × le
  coût de 44,1 → 48 — et il ne concerne qu'une source DSD256 ou 384 kHz jouée
  sur une zone à 44,1 kHz.
* Le délai annoncé (`output_delay`) passe de 32 à **64 trames** (1,45 ms à
  44,1 kHz) pour 352,8 → 44,1 et de 29 à **58 trames** (1,32 ms) pour
  384 → 44,1.
* La ligne 44,1 → 48 donne ici 3,7–3,9 s contre 2,07–2,15 s dans les tableaux
  du 13 et du 15/09 : Shrek était nettement plus chargé (charge 80–90). Ne
  comparer que les lignes d'un même tableau.

**Ce qui n'est pas mesuré** : le coût sur ARM. Un Raspberry Pi 4 ou 5 n'a ni
l'AVX2 ni la fréquence de ce Xeon ; le rapport × 1,6–1,7 entre 1 024 et 512
devrait s'y retrouver (même boucle), mais le pourcentage d'un cœur, non.
Pour le mesurer : compiler `tune-core` en `--release` sur le Pi (ou en
croisé depuis Shrek, cible `aarch64-unknown-linux-gnu`), reprendre la recette
ci-dessus (5 min de stéréo, `rubato_resample_chunk` par blocs de 1 024,
`/usr/bin/time`) pour 352,8 → 44,1 en « prod » et en 512 forcé, et lire
`user`. Tant que ce n'est pas fait, la seule borne connue est celle-ci : si
le Pi est *k* fois plus lent que ce Xeon sur cette boucle, 352,8 → 44,1 lui
coûte 2,7 · *k* % d'un cœur par zone.

Journaux : `/tmp/b14-4080-build-cout.log` sur Shrek ; les deux passages
dans le scratchpad de la session (`b14-4080/cout-noyau-shrek-run{1,2}.log`)
et recopiés dans la PR.
