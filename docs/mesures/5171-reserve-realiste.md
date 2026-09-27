# Réserve de l'égaliseur : Sûre ou Réaliste — #5171

**État : mesuré.** Les chiffres ci-dessous sortent du banc
`mesures_5171` (`sdk/tune-plugin-equalizer/src/engine.rs`, module
`reserve_realiste_5171`) :

```sh
cd sdk && cargo test --release -p tune-plugin-equalizer --lib mesures_5171 -- --ignored --nocapture
```

Relevé sur Shrek le 26/09/2026. Rien n'est écouté ici : ce sont des crêtes,
des niveaux moyens et des compteurs d'échantillons.

## Ce qui change

La réserve « Sûre » (#4073, #4594, `2218-marge-ecretage-crete-vraie.md`)
reste le **défaut**, et elle est identique au bit près : la norme L1 de la
cascade, qui rend l'écrêtage impossible quelle que soit l'entrée.

La réserve « Réaliste » est un **choix** de l'utilisateur, rangé dans le
profil d'égaliseur de la zone (`headroom_mode: "realistic"`) :

1. le pré-gain vaut `−(maximum de la réponse en fréquence + 0,25 dB)`. Le
   maximum est cherché sur une grille log de 8 192 points (1 Hz → Nyquist,
   plus 0 Hz, Nyquist et le centre de chaque bande), puis affiné par
   section dorée. Les 0,25 dB = 0,2 dB d'écart entre la pleine échelle et le
   seuil du limiteur, plus 0,05 dB de garde ;
2. un **limiteur de sécurité** suit la cascade
   (`sdk/tune-plugin-audio-support/src/limiteur.rs`) :
   - seuil −0,2 dBFS : dessous, gain exactement 1, l'échantillon n'est pas
     multiplié ;
   - au-dessus, un gain à genou doux (tanh) qui tend vers −0,01 dBFS sans le
     dépasser : un GAIN, jamais un écrêtage, le même sur tous les canaux ;
   - attaque instantanée, maintien 20 ms, relâchement 150 ms ;
   - **aucun regard en avant : latence nulle.** Un regard en avant retarde le
     signal ; or le processeur est reconstruit à chaque piste (bras réseau) et
     à chaque cran de curseur (sortie locale), et aucun chemin ne le vide en
     fin de piste : chaque enchaînement gapless aurait perdu 1 ms.

La marge de 0,2 dB garantit qu'**aucun signal stationnaire** ne réveille le
limiteur : un sinus à 0 dBFS placé à la fréquence où la courbe pousse le plus
ressort à −0,25 dBFS. Il n'agit que sur les transitoires, la sonnerie des
cloches sur un front, que le maximum fréquentiel ne voit pas et que la norme
L1 couvrait.

## La courbe de Thierry

Graphique 31 bandes, Q = 4,32 (grille ISO du client web) : 20 Hz +3,5,
25 Hz +3,5, 31,5 Hz +3,0, 40 Hz +2,5, 50 Hz +1,5, 63 Hz +1,0, 80 Hz +0,5,
250 Hz −1,0, 315 Hz −1,5, 400 Hz −1,0, 1 kHz +1,0, 1,25 kHz +1,0,
1,6 kHz +2,5, 2 kHz +2,5, 2,5 kHz +1,0, 4 kHz −0,5, 5 kHz −1,5, 8 kHz +1,5,
10 kHz +1,5, 12,5 kHz +1,5, 16 kHz +1,5, le reste à 0.

| débit | réserve sûre (L1) | maximum de la réponse | réserve réaliste | niveau moyen, sûre | niveau moyen, réaliste |
|---|---|---|---|---|---|
| 44,1 kHz | −9,85 dB | +5,02 dB | −5,27 dB | −8,60 dB | −4,02 dB |
| 48 kHz | −9,89 dB | +5,02 dB | −5,27 dB | −8,62 dB | −4,00 dB |
| 96 kHz | −10,13 dB | +5,02 dB | −5,27 dB | −8,79 dB | −3,94 dB |

**À noter.** Le « −8,4 dB » de la capture de #5069 est la ligne
« Égaliseur » de la carte de compensation, c'est-à-dire le **niveau moyen**
(`gain_moyen_db_at`), pas la réserve elle-même : avec cette courbe, la réserve
L1 vaut 9,85 dB et le niveau moyen −8,60 dB. L'écart de 0,2 dB avec la capture
vient d'une courbe qui n'est pas exactement celle de l'écran de Thierry (la
capture n'était pas jointe au ticket).

La réserve réaliste rend **4,6 dB** de niveau moyen sur cette courbe. La
compensation de niveau demande autant de moins au volume.

## Action du limiteur (10 s par signal, 44,1 kHz, stéréo)

| signal | sûre : crête / overs | réaliste sans limiteur : crête / overs | réaliste : trames limitées | réduction max | crête de sortie | overs |
|---|---|---|---|---|---|---|
| bruit rose, crête 0 dBFS | −8,57 dBFS / 0 | −3,99 dBFS / 0 | 0 (0 %) | 0 dB | −3,99 dBFS | 0 |
| bruit rose, crête −3 dBFS | −11,57 dBFS / 0 | −6,99 dBFS / 0 | 0 (0 %) | 0 dB | −6,99 dBFS | 0 |
| sinus balayé 20 Hz–20 kHz, 0 dBFS | −4,86 dBFS / 0 | −0,28 dBFS / 0 | 0 (0 %) | 0 dB | −0,28 dBFS | 0 |
| carré 20 Hz pleine échelle | −3,13 dBFS / 0 | +1,45 dBFS / 324 420 | 438 659 (99,5 %) | −1,46 dB | −0,01 dBFS | 0 |
| carré 50 Hz pleine échelle | −5,49 dBFS / 0 | −0,91 dBFS / 0 | 0 (0 %) | 0 dB | −0,91 dBFS | 0 |
| grosse caisse 45 Hz pleine échelle, 2 coups/s | −8,01 dBFS / 0 | −3,43 dBFS / 0 | 0 (0 %) | 0 dB | −3,43 dBFS | 0 |
| master « guerre du volume » (rose ×4 écrêté à 0 dBFS) | −4,45 dBFS / 0 | +0,13 dBFS / 3 | 5 090 (1,15 %) | −0,15 dB | −0,02 dBFS | 0 |
| carré 1 kHz pleine échelle | −6,35 dBFS / 0 | −1,77 dBFS / 0 | 0 (0 %) | 0 dB | −1,77 dBFS | 0 |
| signal adverse `signe(h[−n])`, pleine échelle | −0,27 dBFS / 0 | +4,31 dBFS / 34 957 | 306 865 (69,6 %) | −4,32 dB | −0,01 dBFS | 0 |
| sinus 0 dBFS au maximum de la courbe | −4,83 dBFS / 0 | −0,25 dBFS / 0 | 0 (0 %) | 0 dB | −0,25 dBFS | 0 |

Lecture :

- sur de la musique ordinaire (bruit rose, balayage, grosse caisse), le
  limiteur **n'agit jamais** ;
- sur un master très compressé, il agit sur 1,15 % des trames, de 0,15 dB au
  plus ;
- il travaille franchement sur deux signaux de laboratoire : un carré de
  20 Hz, dont les harmoniques tombent toutes dans les bandes relevées
  (1,46 dB au plus), et le signal adverse construit pour atteindre la
  norme L1 (4,32 dB au plus). Dans tous les cas, **zéro échantillon au rail**.

## Où le voir

- Chemin du signal : l'étape DSP dit « EQ actif (réserve sûre, pré-gain auto
  −9.8 dB, sans limiteur) » ou « EQ actif (réserve réaliste, pré-gain auto
  −5.3 dB, limiteur de sécurité, n'a pas agi) » / « …, a agi sur N trames
  (x %) depuis le démarrage, −y dB au plus ». Le champ `eq_headroom` en donne
  le détail (`mode`, `reserve_db`, `limiter`). Le compteur est celui du
  **processus**, comme les compteurs d'écrêtage de #2218 : il cumule les
  zones en réserve réaliste depuis le démarrage.
- Journal : une ligne `dsp_limiteur` au premier bloc limité d'une piste, et
  une à sa fin avec le total.
- Diagnostics du greffon : `headroom_mode` et `limiter`.

## Limites connues

- Le greffon **natif** signé (s'il est installé à la place du moteur
  embarqué) tient son propre compteur : le limiteur y agit, mais son compteur
  n'atteint pas le registre du serveur.
- Les routes « correction de pièce » et « calibration DAC » écrivent un
  profil neuf : elles ramènent la réserve à « Sûre ».
