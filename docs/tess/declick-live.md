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
4. **Candidat** : suite où `|ẽ| > K·σ` (`K` = `threshold`, **18** par
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
7. **Interpolation LSAR** (Godsill et Rayner) : les `m` échantillons du cœur
   minimisent `Σ e[n]²` sur `[s, s+m+p)`, `p` échantillons connus de chaque
   côté. Système `R x_u = −b`, `R` Toeplitz en bande, Cholesky en bande,
   `O(m·p²)`, sans allocation.

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
- **Détection** (`tests/detection.rs`) : 283 clics (impulsions et clics
  amortis de 0,05 à 1 ms, −40 à −6 dBFS) dans un programme d'environ
  −20 dBFS RMS ; taux par niveau, fausses réparations, gain de SNR local.
- **CPU et latence** : `cargo run --release -p tune-plugin-declick-live
  --example banc`. À refaire sur le Raspberry Pi 4 (.44, Cortex-A72) en
  stéréo à 44,1 / 96 / 192 kHz pour fixer le plafond de cadence.

## 6. Limites connues

- Le seuil par défaut est prudent : il trouve tous les clics au-dessus de
  −20 dBFS, environ la moitié (44,1 kHz) à 90 % (96 kHz) entre −30 et
  −20 dBFS, presque aucun en dessous, sous une musique à −20 dBFS RMS.
- Sous des cymbales ou des attaques de clavecin, les clics sont surtout
  laissés en place (le greffon préfère ne rien faire).
- Sur piano seul, la réparation LSAR d'ordre 32 améliore mal le signal
  (gain local négatif mesuré à 48 kHz) : à reprendre (ordre plus élevé ou
  modèle ré-estimé des deux côtés) avant la mise en avant de la fonction.
- Stéréo : pas de détection conjointe des canaux.
- Une attaque instantanée en marche d'escalier (synthèse sans continuité)
  est indiscernable d'un clic par cette méthode.

## 7. Hors périmètre de ce lot

Branchement dans l'hôte (chaîne, catalogue, écran), emballage F64 du SDK
(T05), droit d'accès : le descripteur déclare le droit Premium existant
`declick` (décision P1), le système de licences n'est pas touché.
