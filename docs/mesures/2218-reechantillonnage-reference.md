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

82 dans la cible : **67 passés, 15 ignorés** (D1 : 5, D2 : 7, D3 : 3), 0
rouge. Par rapport : `delai_residuel`, `erreur_rms_sinus_1k`,
`erreur_rms_balayage`, `bande_passante`, `rejection`, `bords`,
`longueur_et_vidage`, `blocs_de_1024_et_4096_rendent_la_piste_a_l_identique`
(×7), les seuils audiophiles `audiophile_bande_20k_*` (7, 5 ignorés),
`audiophile_erreur_1k_*` (7, 3 ignorés), `audiophile_delai_residuel_nul`
(×7, tous ignorés), les 4 preuves de la référence, et le relevé
(`releve_de_tous_les_rapports`, imprime tout avec `--nocapture`).

## Cases de #2218 (« SRC, remix et DSP »)

* « Nombre exact de frames pour chaque ratio et flush » — déjà cochée ;
  confirmée ici contre une référence (longueur exacte, vidage déterministe),
  **avec une réserve** : exact en nombre, pas en position (D2).
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
3. **« SRC 192 → 44,1 : erreur en bande −87,8 dB (gain +0,00036 dB), sous le
   seuil de 24 bits »** — noyau 512, ondulation de bande passante ; 96 → 48
   et 176,4 → 48 à −99 dB. Pas de distorsion (THD+N < −143 dB). Piste :
   `oversampling_factor` ou fenêtre. Dé-ignorer `audiophile_erreur_1k_*`.

## Reproduction

```sh
cargo test -p tune-core --test reechantillonnage_reference_2218
cargo test -p tune-core --test reechantillonnage_reference_2218 -- --ignored   # les 15 défauts
cargo test -p tune-core --test reechantillonnage_reference_2218 releve -- --nocapture
```
