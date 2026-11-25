# Declick temps réel (`declick-live`) : note de méthode

Lot TESS T13. Greffon DSP Premium, id `declick-live`, distinct de l'outil
`declick` (« Dé-ploc ») qui travaille par lots sur des fichiers.
Caisse : `sdk/tune-plugin-declick-live`. Calcul interne en `f64`.

## 1. Modèle

Autour d'un clic, la musique est bien décrite par un modèle autorégressif
(AR) d'ordre `p = 32` : `e[n] = Σ_{k=0..p} a_k x[n-k]`, avec `a_0 = 1`.
Un clic est une perturbation brève et additive que ce modèle ne prédit pas.

## 2. Chaîne, par canal

Le signal traverse une ligne à retard de `L` trames (anticipation). Toutes les
`H` trames (4 ms, 2 ms si l'anticipation est courte), une « passe » décide
des `H` trames les plus anciennes encore modifiables, `F = [T-L, T-L+H)`.

1. **Modèle AR** : estimé sur les 20 ms passées (déjà restaurées), fenêtre
   de Hann, autocorrélation puis Levinson-Durbin. Fenêtre quasi silencieuse
   (sous −120 dBFS) : la passe ne détecte rien.
2. **Erreur de prédiction** `e[n]` et **filtrage adapté**
   `ẽ[n] = Σ a_k e[n+k]` (erreur « bilatérale », Vaseghi).
3. **Échelle de référence** : `σ = 1,4826 × médiane |ẽ|` sur les 5 ms
   passées, avec un plancher : le bruit de quantification d'un 16 bits
   (−101 dBFS). Sans lui, un enregistrement 24 bits ou synthétique est si
   prévisible que toute note nouvelle dépasse le seuil de cent fois.
4. **Candidat** : suite où `|ẽ| > K·σ` (`K` = `threshold`, **8** par
   défaut, 4 à 30), trous de 8 échantillons au plus, qui commence dans `F`.
5. **Rejets** (protection de la musique) ; le candidat reste intact si :
   - il dépasse `max_click_ms` + `2p` échantillons (événement long) ;
   - l'échelle robuste (médiane) de `ẽ` avant ou après lui dépasse `2,5·σ`
     (changement de régime : attaque de piano, de cymbale) ;
   - le RMS de `ẽ` après lui dépasse `1,6·σ` : la médiane ignore une suite
     de pics clairsemés, comme les coins d'une corde nouvellement pincée ;
   - son pic n'atteint pas `K` fois le maximum de `σ` et de ces trois
     échelles locales ;
   - un modèle AR ajusté sur les 8 ms qui le suivent prédit le signal plus
     de 3 fois mieux que le modèle passé (le spectre a changé : un son
     nouveau commence) ;
   - il a commencé dans des trames déjà jouées, ou sa fin n'est pas visible.
6. **Cœur du clic** : l'erreur avant ne s'étale qu'APRÈS le clic, l'erreur
   arrière (`Σ a_k x[n+k]`) qu'AVANT. Le cœur va du premier échantillon où
   l'erreur avant atteint 3 % de son maximum au dernier où l'erreur arrière
   l'atteint. Seul ce cœur (± 2) est interpolé : interpoler tout l'étalement
   du filtre adapté (± `p`) dégradait le signal plus que le clic lui-même.
7. **Interpolation LSAR** (Godsill et Rayner) avec un **modèle des deux
   côtés** : les autocorrélations du passé et des 8 ms qui suivent le clic
   sont mises en commun, avec un léger élargissement de bande
   (`a_k ← 0,998^k a_k`) pour éloigner les pôles du cercle unité. Les `m`
   échantillons du cœur minimisent `Σ e[n]²` sur `[s, s+m+p)`. Système
   `R x_u = −b`, Toeplitz en bande, Cholesky en bande, sans allocation.
8. **Garde-fous de la réparation** : elle n'est jamais pire que l'original.
   La réparation est annulée, l'original gardé, si :
   - **validation croisée** : le même interpolateur (même factorisation),
     appliqué à un trou de même longueur dans le signal propre juste avant
     et juste après, s'y trompe de `E_cv` ; la réparation doit retirer au
     moins `4·E_cv`, sinon elle ajouterait plus d'erreur qu'elle n'en ôte ;
   - **débordement** : elle retire plus de 16 fois l'énergie que le clic
     explique (excès d'erreur avant `/ R(0)`) : elle réécrit la musique ;
   - **modèles indépendants** : elle n'abaisse pas l'erreur de prédiction
     sous le modèle passé ET sous le modèle de l'après ;
   - **enveloppe** : un échantillon réparé dépasse 1,5 fois le maximum de
     son contexte.

## 3. Anticipation et latence

`L = lookahead_ms` (10 à 20 ms, 15 par défaut), annoncée à l'hôte par
`latency_frames()`. Elle couvre : la passe, le plus long candidat, `2p`
échantillons de contexte et la fenêtre de contrôle (3 ms au plus). `prepare`
refuse les cadences sous 32 kHz. Changer `lookahead_ms` à chaud est refusé
(la latence changerait) ; `threshold` et `max_click_ms` changent à chaud.
`drain` rend les `L` trames retenues ; la fin du flux n'est jamais prise
pour un clic (aucun contexte « après » fabriqué).

## 4. Garanties

- **Hors clic, sortie = entrée retardée de `L`, au bit près** (S16, S24, S32,
  F32, F64). Seuls les échantillons d'un clic accepté sont requantifiés.
- Fil audio : aucune allocation, aucun verrou, aucune E/S ; tout est alloué
  dans `prepare`. Les 35 premières ms après un `reset` ne sont pas traitées.
- Canaux traités indépendamment (1 à 64).

## 5. Preuve (tests de la caisse, corpus généré, aucun fichier protégé)

- **Contre-épreuve** (`tests/contre_epreuve.rs`) : clavecin (Karplus-Strong,
  tessiture FF1–f3, corde pincée triangulaire, arpèges rapides et voix
  nombreuses), cymbales (partiels inharmoniques, attaque nulle), piano
  (partiels inharmoniques, feutre, marteau), 20 s stéréo, 44,1 et 96 kHz,
  S16, S24 et F32 : **zéro échantillon modifié**.
- **Générateurs durs, hors contre-épreuve** (`harpsichord_hard`,
  `piano_hard`) : les premiers générateurs du lot (clavecin excité au bruit
  blanc jusqu'à 3,5 kHz ; piano sans feutre, rampe linéaire, choc large
  bande). Mesurés seulement : 0 échantillon modifié à 44,1 et 96 kHz (S16).
- **Détection** (`tests/detection.rs`) : clics impulsions et amortis de 0,05
  à 1 ms, −40 à −6 dBFS, dans un programme d'environ −20 dBFS RMS ; taux par
  niveau, fausses réparations, gain de SNR local, clics aggravés.
- **CPU et latence** : `cargo run --release -p tune-plugin-declick-live
  --example banc`. À refaire sur le Raspberry Pi 4 (.44, Cortex-A72) en
  stéréo à 44,1 / 96 / 192 kHz pour fixer le plafond de cadence.

### Mesures, itération 1 (tête `a3f060aa7`) → itération 2

Programme, détection par niveau :

| | −40…−30 | −30…−20 | −20…−6 | fausses | gain local |
|---|---|---|---|---|---|
| 44,1 kHz, it. 1 (K = 18) | 0 % | 54 % | 100 % | 0 | +5,3 dB |
| 44,1 kHz, it. 2 (K = 8) | 19 % | **87 %** | 99 % | 0 | **+9,3 dB** |
| 96 kHz, it. 1 | 4 % | 89 % | 100 % | 0 | +4,1 dB |
| 96 kHz, it. 2 | 56 % | 97 % | 97 % | 0 | +4,6 dB |

Matériel difficile avec clics (48 kHz, 120 clics) :

| corpus | it. 1 : détectés / gain | it. 2 : détectés / gain / clics aggravés |
|---|---|---|
| piano | 92–97 % / **−6,6 dB** | 17–71 % / **+3,6 dB** / 1 |
| clavecin | 52–68 % / −3,7 dB | 37–48 % / +2,6 dB / 4 |
| cymbales | 0–3 % / +0,1 dB | 0–8 % / +0,4 dB / 0 |
| piano dur (hors c.-é.) | — | 5–65 % / +4,3 dB / 1 |
| clavecin dur (hors c.-é.) | — | 9–21 % / +1,4 dB / 0 |

CPU (Shrek, Xeon E5-2630 v4, un cœur, F32 stéréo, blocs de 512, 15 ms) :
44,1 kHz 4,7 % (pire bloc 8 %), 96 kHz 8,8 % (22 %), 192 kHz 20 % (68 %).

## 6. Limites connues

- **Sous des cymbales** : presque rien n'est réparé (0 à 8 %). Le bruit large
  bande d'une cymbale masque l'erreur d'un clic ; le greffon s'abstient.
- **Près des attaques** (clavecin, piano) : un clic situé dans les quelques
  millisecondes d'une attaque est laissé en place ; les tests d'isolation et
  de changement de spectre protègent l'attaque au prix de ce clic.
- **Piano et clavecin seuls** : les garde-fous refusent beaucoup de
  réparations (l'ordre 32 prédit mal ces signaux très riches) ; le gain
  reste positif mais la détection effective baisse (17 à 71 % sur piano).
- **Clics aggravés** : le gain est positif sur chaque corpus, mais quelques
  clics isolés (1 à 6 par corpus de 120 à 283) finissent un peu plus loin
  du signal propre qu'avant : réparation partielle d'un clic amorti.
- **Sous −30 dBFS** à 44,1 kHz, sous une musique à −20 dBFS RMS : 19 %.
- Stéréo : pas de détection conjointe des canaux.

## 7. Hors périmètre de ce lot

Branchement dans l'hôte (chaîne, catalogue, écran), emballage F64 du SDK
(T05), droit d'accès : le descripteur déclare le droit Premium existant
`declick` (décision P1), le système de licences n'est pas touché.
