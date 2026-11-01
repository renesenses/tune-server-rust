# Étude #7632 — Annexe B : résultats détaillés du prototype

*Sortie de `tools/tune-calibration` (`cargo run --release --bin calib-demo`) sur Shrek, 01/11/2026. Renvoi depuis [la note principale](7632-calibration.md), § 3.*

## 3. Prototype : résultats sur pièces simulées

Crate `tools/tune-calibration/` (Rust 2021, `realfft 3.5`, `rustfft 6.4`, `serde`, `serde_json` ; toutes MIT/Apache-2.0, `cargo deny check licenses` vert). 33 essais unitaires + 8 essais d'intégration (`tests/pieces_simulees.rs`), tous verts sur Shrek (`cargo test --release`).

Pièces simulées (`sim::RoomSim`) : son direct (gain 0,3, retard 12 ms), réflexion de sol (3,5 ms, −0,12), trois modes paramétriques (42 Hz +9 dB Q6, 68 Hz +6 dB Q5, 115 Hz +5 dB Q7), bruit de fond −80 dBFS ; variante réverbérante avec queue diffuse par octave (RT60 de 0,9 s à 63 Hz à 0,30 s à 4 kHz). Le balayage est « capté » par convolution : **aucun son n'est joué ni capté**.

### 3.1 Mesure (C1)

`calib-demo` à 48 kHz, balayage 10 Hz – 21,6 kHz de 6 s à −12 dBFS, fenêtre 100 ms / 500 ms, lissage 1/12 oct, grille 48 pts/oct ; machine Shrek, un cœur.

| Grandeur | Pièce sèche | Pièce réverbérante | Attendu |
|---|---|---|---|
| Retard retrouvé | 576 éch. = 12,000 ms | 576 éch. = 12,000 ms | 12 ms, au sample près |
| Crête / bruit de l'IR | 72,1 dB | 72,1 dB | bruit de fond −80 dBFS |
| Écart mesure − réponse vraie, 30 Hz – 10 kHz | < 0,3 dB (essai `la_mesure_retrouve…`) | — | — |
| Modes retrouvés comme maxima locaux (±4 %) | 42, 68, 115 Hz | 42, 68 Hz (115 Hz noyé dans l'ondulation du champ diffus) | 42, 68, 115 Hz |
| Durée de l'analyse (déconvolution 6 s + FFT + RT60 10 bandes + lissage) | 400 ms | 499 ms | — |

Deux défauts ont été trouvés et corrigés par ces essais : (1) la normalisation de l'inverse par la crête temporelle donnait +1 dB uniforme (crête d'une impulsion limitée à 21,6/24 de la bande) : normalisation par la magnitude spectrale en bande ; (2) une fenêtre gauche de 5 ms tronquait la pré-oscillation du noyau de déconvolution (bord 10–20 Hz, ~100 ms) : 0,4 dB d'erreur entre deux modes, 0,35 dB à 30 Hz ; fenêtre gauche portée à 100 ms (REW : 125 ms), erreur < 0,3 dB partout. Une compensation explicite de la réponse du noyau a été essayée : sans effet, retirée.

**RT60 par octave** (pièce réverbérante ; la queue diffuse commence à 125 Hz, en dessous la décroissance est celle des modes) :

| Octave (Hz) | EDT (s) | T20 (s) | T30 (s) | RT60 simulé (s) |
|---|---|---|---|---|
| 63 | 0,32 | 0,71 | 0,73 | — (modes Q5-Q6) |
| 125 | 0,35 | 0,62 | 0,63 | 0,70 |
| 250 | 0,25 | 0,63 | 0,62 | 0,55 |
| 500 | 0,26 | 0,53 | 0,49 | 0,45 |
| 1000 | 0,24 | 0,45 | 0,40 | 0,40 |
| 2000 | 0,18 | 0,35 | 0,34 | 0,35 |
| 4000 | 0,16 | 0,30 | 0,30 | 0,30 |

T30 à ±10 % de 500 Hz à 4 kHz ; +13 % à 250 Hz et −10 % à 125 Hz, parce que les flancs des filtres d'octave mélangent les bandes voisines à décroissances différentes (un Butterworth d'ordre 6 fera mieux). L'EDT est plus court que T30 : le son direct domine les 10 premiers dB (rapport direct/réverbéré positif), c'est le comportement attendu. La cascade montre une chute moyenne > 6 dB entre la tranche 0 et la tranche 270 ms (essai `le_rt60_par_bande_est_retrouve`).

**Étalonnage micro** : une coloration de −3 dB à 150 Hz (Q 1,5) et +4 dB de shelf à 6 kHz, décrite par un fichier UMIK synthétique de 60 points, est annulée à 0,25 dB près de 30 Hz à 15 kHz (essai `l_etalonnage_du_micro_annule_sa_coloration`).

**Synchronisation sans horloge commune** : capture commencée 31 337 éch. avant la lecture, repère de 100 ms : origine retrouvée exactement, retard résiduel du balayage ≤ 1 éch. (essai `le_repere_de_synchronisation…`).

### 3.2 Égaliseur automatique (C2)

Cible plate, zone 20–300 Hz, alignement au 30e percentile sur 40–300 Hz, lissage 1/6 avant ajustement, 8 bandes max, coupe cumulée ≤ 12 dB, aucune relance.

**Pièce sèche** — 3 bandes en 26 ms :

| Bande | f (Hz) | gain (dB) | Q | Mode simulé |
|---|---|---|---|---|
| 1 | 42,1 | −8,09 | 10,0 | 42 Hz, +9 dB, Q 6 |
| 2 | 68,3 | −5,52 | 7,7 | 68 Hz, +6 dB, Q 5 |
| 3 | 114,8 | −4,99 | 8,4 | 115 Hz, +5 dB, Q 7 |

Pic au-dessus de la cible : +5,90 → +0,56 dB ; dépassement efficace 1,98 → 0,13 dB ; gain cumulé max 0 dB ; préampli 0 dB. **Mesure de vérification** (balayage rejoué à travers l'égaliseur, puis remesuré) :

| f (Hz) | 30 | 42 | 50 | 68 | 80 | 100 | 115 | 150 | 200 | 300 |
|---|---|---|---|---|---|---|---|---|---|---|
| avant (dB) | −1,38 | +7,07 | +0,80 | +4,98 | +1,10 | +0,99 | +4,61 | +0,22 | −0,71 | −2,20 |
| après (dB) | −1,29 | **−0,01** | +0,16 | **−0,03** | +0,38 | +0,27 | **+0,21** | +0,25 | −0,47 | −1,91 |

Les fréquences sont retrouvées à 0,3 % près ; les gains sont de 1 à 1,5 dB moins profonds que les modes et les Q plus élevés, parce que la cible passe au 30e percentile, c'est-à-dire un peu au-dessus du plancher entre les modes : on coupe ce qui dépasse, pas le mode entier. Les creux (30 Hz, 300 Hz) ne bougent pas.

**Pièce réverbérante** — 4 bandes en 53 ms : 42,1 Hz −7,1 dB ; 67,3 Hz −4,9 dB ; 102,3 Hz −8,1 dB ; 239,7 Hz −6,3 dB. Vérification : 42 Hz +6,5 → +0,6 ; 68 Hz +4,4 → +0,5 ; 100 Hz +6,0 → −0,1 ; mais 115 Hz +6,2 → +5,2 (le pic voisin à 102 Hz, créé par le champ diffus, a été pris pour cible, et la bande suivante à 115 Hz n'améliore pas assez le coût lissé à 1/6). Dépassement efficace 2,50 → 0,57 dB, avertissement « pic résiduel 3,2 dB » émis. C'est le cas d'école qui justifie la **moyenne spatiale** (C4) : en une seule position, une partie des pics au-dessus de ~100 Hz est du champ diffus propre à ce point.

**Garde-fous** (essai `les_garde_fous_de_l_egaliseur_tiennent`, pic de +18 dB et creux de −12 dB, plafond 8 dB) : aucune relance, coupe cumulée ≥ −8 dB en tout point, préampli 0, fréquences et Q dans les bornes, creux non rempli, avertissement émis. Deux défauts d'algorithme trouvés par les essais : l'alignement de la cible par la moyenne (tirée vers le haut par les pics : sous-correction) → médiane puis percentile ; la comparaison d'une mesure lissée à une réponse de biquad non lissée (Q trop élevés, coupes trop faibles) → la réponse de l'égaliseur est lissée de même dans le résidu.

### 3.3 FIR et coût (esquisse C3)

FIR phase mixte de 16 384 taps conçu en 9 ms à partir des bandes de la pièce sèche (phase minimale sous 300 Hz, identité au-dessus) ; écart FIR − égaliseur ≤ 0,03 dB à 30, 42, 68, 115, 200, 300 Hz, 0,00 dB à 1 et 5 kHz.

| fs (Hz) | taps | partitions | latence bloc (ms) | MFLOP/s par canal | FDL (Kio) |
|---|---|---|---|---|---|
| 44 100 | 16 384 | 16 | 23,2 | 15 | 128 |
| 48 000 | 16 384 | 16 | 21,3 | 17 | 128 |
| 48 000 | 65 536 | 64 | 21,3 | 35 | 512 |
| 96 000 | 32 768 | 32 | 10,7 | ~45 | 256 |
| 192 000 | 65 536 | 64 | 5,3 | 141 | 512 |

Estimation analytique (produits spectraux + 2 FFT de 2048 par bloc), pas un bench : un RPi 4 tient ~2 GFLOP/s en f32, soit < 10 % d'un cœur dans le pire cas ; l'allocation par bloc de `process_block` pèse plus que le calcul et reste à corriger.

