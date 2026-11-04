# Étude #7614 — greffon de suréchantillonnage haute qualité (rc11)

Claude - Mozaiklabs, 31 octobre – 4 novembre 2026. **État : étude close, § 6 mesuré sur Shrek le 2 novembre ; intégration engagée (go du 4 novembre).** Branche
`etude/7614-surechantillonnage-b209` (base `batch/bugs-rc10-20261030`).
Prototype : `plugins/tune-surechantillonnage/` (crate autonome, hors serveur).
Mesures reproductibles : `cargo run --release --bin banc` dans la crate ;
gabarits : `cargo test --release`.

L'idée vient d'un membre de la communauté formé au traitement du signal ; les
conceptions ci-dessous sont les nôtres, bâties sur la littérature publique
citée en fin de note. Aucune conception propriétaire n'est reprise ni nommée
comme modèle.

## 1. Objet et périmètre

Offrir, à qui le choisit, un suréchantillonnage PCM de qualité contrôlée vers
la cadence la plus haute que la sortie accepte (352,8 / 384 / 705,6 / 768 kHz),
avec un dither et une mise en forme du bruit à la réduction de profondeur.
PCM d'abord ; le DSD (sortie DSD native par modulateur) n'est pas traité ici.
Le greffon est désactivé par défaut, incompatible avec le mode PURE, affiché
comme tel dans le chemin du signal. Rien n'entre dans un lot avant le go
de Bertrand sur l'implémentation.

## 2. État des lieux dans Tune

**Rééchantillonnage actuel.** `tune-core/src/audio/resample.rs` construit un
`rubato::Async` (sinc, interpolation linéaire entre 256 phases, fenêtre
Blackman², `vendor/rubato` 3.0.0 avec deux correctifs : sommation de Kahan
pour la normalisation, table indépendante de la libm). La longueur du noyau
(128, 256, 512 ou 1 024) est choisie pour tenir −0,1 dB à 20,5 kHz quelle que
soit la paire de cadences (#2218, D1). Il n'est **pas** conçu pour monter en
cadence par choix : il sert quand le périphérique n'accepte pas la cadence de
la source (`decide_local_rate_opening`, `outputs/local/resolution.rs`), ou
pour le convertisseur de fichiers. Mesuré à 256 coefficients sur 44,1 → 48 :
20 750 Hz à −0,1 dB, −116 dB de réjection. La réjection « −111 à −146 dB »
est celle d'une table à 256 phases interpolée linéairement ; c'est le plafond
de cette structure, qu'un polyphase exact à `L` phases n'a pas.

**DSD → PCM.** `audio/dsd_to_pcm.rs` : décimation FIR par sinc fenêtré
Blackman-Harris (256 à 2 048 coefficients selon le rapport, coupure
`0,45 / D`), sortie 176,4 ou 352,8 kHz, gain SACD ×2, calcul par lots de huit
accumulateurs (#4354). Le PCM issu du DSD est déjà à haute cadence : le
greffon n'a rien à lui apporter, sauf si la sortie est plus haute encore.

**Chaîne DSP de la sortie locale.** `outputs/local.rs::apply_local_dsp`
applique, en f32 et **à la cadence de la source**, dans cet ordre : égaliseur
→ convolveur de correction de pièce → crossfeed (stéréo) → repli mono. Puis
`convertir` adapte les canaux, rééchantillonne avec rubato **si et seulement
si** le périphérique l'exige, et quantifie vers le format du puits
(`LocalPcmKind`). Le dither de réduction de profondeur est celui, unique et
déterministe, de `tune-plugin-audio-support::dither` (TPDF, graine dérivée du
contenu du bloc, #4075/#4076) ; il n'y a **aucune mise en forme du bruit**
dans le dépôt aujourd'hui. Sous DoP ou PURE, `apply_local_dsp` rend la main
sans toucher aux échantillons.

**Greffons DSP du SDK.** `sdk/tune-plugin-sdk/src/audio.rs` définit
`DspFactory::prepare(format, max_frames, settings) -> Box<dyn Processor>` et
`Processor::process(&mut AudioBlock, BlockContext)`. Le contrat est **en
place, à format constant** : un bloc entre à `format.sample_rate()` et ressort
avec le même nombre de trames et la même cadence. `AudioFormat::new` accepte
jusqu'à 768 000 Hz. Les greffons natifs existants (`tune-plugin-equalizer`,
`tune-plugin-crossfeed`, embarqués dans `tune-core` ; `declick`, `converter`
dans `tune-server`) respectent ce contrat. **Un suréchantillonneur change la
cadence et le nombre de trames : il ne tient pas dans `Processor` tel quel.**
C'est la contrainte structurante de l'intégration (§ 7).

**Mode PURE et chemin du signal.** `pure_bypass` (`AtomicBool`, rafraîchi en
vol) court-circuite tout le DSP ; `OutputSignalPathStatus`
(`tune-output-api`) porte `bit_perfect`, `sample_transport`,
`dsp: OutputDspState::{Inactive, Applied, BypassedPure, BypassedDop, Unknown}`
et des `reasons` (`DspApplied`, `FloatTransport`, `SoftwareVolume`…). Le
greffon actif devra poser `dsp = Applied`, `bit_perfect = false` et une raison
nouvelle (`Upsampled { from, to }`), et rester inactif dès que `pure_bypass`
est vrai.

**Cadences acceptées par type de sortie** (ce que le code dit ; à confirmer
sortie par sortie à l'étape I1) :

| sortie | cadence de sortie | source dans le code |
|---|---|---|
| locale (ALSA, CoreAudio, WASAPI, ASIO) | celle du périphérique ; ouverture à la cadence source si elle est mesurée acceptée, sinon rééchantillonnage vers la cadence par défaut | `outputs/local/resolution.rs::decide_local_rate_opening` |
| AirPlay | 44,1 kHz fixe, 16 bits | `outputs/airplay.rs` (`SAMPLE_RATE = 44100`) |
| OAAT / HTTP WAV, DLNA, OpenHome, Squeezebox, Chromecast, BluOS | la cadence de la source, plafonnée par `max_sample_rate` de la zone (catalogue de périphériques + réglage) | `orchestrator/resolve_stream.rs` (« zone_max_sample_rate_cap_applied ») |
| HQPlayer | l'hôte suréchantillonne lui-même | `outputs/hqplayer.rs` |

Le greffon n'a donc de sens que là où une cadence **supérieure** à la source
est acceptée : la sortie locale (périphérique qui annonce 352,8/384/705,6/768),
et les sorties réseau dont le plafond de zone le permet. AirPlay et HQPlayer
l'excluent d'office.

## 3. Conception des filtres

### 3.1 Méthode commune

Tous les filtres sont des **sinc fenêtrés** (Oppenheim & Schafer, chap. 7),
conçus à la cadence interpolée `L × Fs_in`, de longueur `portée × L`, puis
découpés en `L` phases de `portée` coefficients (Crochiere & Rabiner, chap. 3).
Le coût par échantillon de **sortie** et par canal est donc `portée`
multiplications-accumulations, indépendamment de `L`. Les coefficients sont
en f64, normalisés par sommation compensée pour que chaque phase ait un gain
continu exactement unitaire (sinon le gain ondule à la cadence d'entrée).

Un filtre est décrit par sa portée (en échantillons d'entrée), sa coupure
(fraction de `Fs_in`), sa fenêtre et sa phase. Les constantes de conception
sont dans `conception.rs::catalogue`.

**Rapports entiers** (44,1 → 352,8 = ×8 ; 44,1 → 705,6 = ×16 ; 48 → 384 = ×8 ;
48 → 768 = ×16) : un seul étage.

**Rapports non entiers** (44,1 → 384, 48 → 352,8, 44,1 → 768, 48 → 705,6) :
deux étages. L'étage 1 est le filtre de la famille au facteur entier
`⌊vers/de⌋` ; l'étage 2 est un sinc court rationnel `L2/M2` (par exemple
160/147 pour 352,8 → 384) dont la bande passante s'arrête à `de/2` et la bande
atténuée commence à la première image `intermédiaire − de/2` : la transition
occupe presque toute la cadence intermédiaire et 16 coefficients par phase
suffisent à 150 dB (Kaiser). Son coût est négligeable et sa latence de l'ordre
de 20 µs. Le filtre de la famille garde ainsi exactement la même réponse
quelle que soit la cadence de sortie, entière ou non.

### 3.2 Les quatre conceptions, et leurs variantes en phase minimale

| id | fenêtre | portée | coupure | ce qu'elle promet |
|---|---|---|---|---|
| `polyvalent` | gaussienne, α = 5 | 256 | 0,468 Fs | **Apodisant** : −0,1 dB à 20 kHz, plus de 110 dB à 21,9 kHz, donc déjà éteint AVANT Nyquist à 44,1 kHz. Réponse hors bande **monotone** (le spectre d'une gaussienne est gaussien, sans lobes). Éteint la sonnerie du filtre de l'ADC ou du mastering à 22 kHz (Craven 2004). |
| `precision` | Kaiser, β = 15,57 (A = 150 dB) | 1 024 | 0,5 − Δ/2 | Bande plate à 10⁻⁷ dB jusqu'à 21,6 kHz, 150 dB au-delà de Nyquist ; transition de 430 Hz à 44,1 kHz (`Δ = (A−8)/(2,285·2π·portée)`). |
| `precision-longue` | Kaiser, β = 15,57 | 4 096 | 0,5 − Δ/2 | Transition de 107 Hz, −0,1 dB à 21,9 kHz. Quatre fois le coût ; sert à trouver la limite de la machine. |
| `resolution` | gaussienne, α = 5 | 48 | 0,5 Fs | **Transition étendue** : −6 dB à Nyquist, −3 dB vers 21 kHz, −0,1 dB vers 18,7 kHz, 120 dB vers 29 kHz. Réponse impulsionnelle de 1,1 ms sans pré-écho visible ; laisse passer une partie des images entre 22 et 29 kHz, ce que le banc chiffre. |
| `*-pm` | les mêmes | | | **Phase minimale** par le cepstre réel (Oppenheim & Schafer § 13.6) : module conservé, toute l'énergie devant, pas de pré-écho, latence très réduite, phase non linéaire. Le plancher numérique du procédé borne la bande atténuée vers −170 dB. |

Pourquoi la gaussienne pour l'apodisant : à longueur égale, Kaiser ou
Blackman-Harris transitent un peu plus vite mais leur réponse hors bande
présente des lobes ; la gaussienne décroît de façon monotone, ce qui donne une
réponse impulsionnelle sans oscillation résiduelle lointaine. Le choix
`α = 5` tronque la fenêtre à e⁻¹²·⁵ (−109 dB), et le sinc y vaut déjà ~1/100 :
la discontinuité est sous −150 dB. Avec `σ_t = portée/10 = 25,6` échantillons,
`σ_f = Fs/(2π σ_t) = 274 Hz` à 44,1 kHz ; de −0,1 dB (2,28 σ_f) à −120 dB
(4,75 σ_f) il faut 7 σ_f = 1,9 kHz : coupure à 20,63 kHz, −0,1 dB à 20,0 kHz,
−120 dB à 21,9 kHz.

### 3.3 Coût théorique et latence

| filtre | MAC / trame de sortie / canal | 352,8 kHz stéréo | 768 kHz stéréo | latence (phase linéaire) à 44,1 kHz | mémoire des phases (×8 / ×16 / 160/147) |
|---|---|---|---|---|---|
| polyvalent | 256 | 0,18 GMAC/s | 0,39 GMAC/s | 2,89 ms | 16 Kio / 32 Kio / +20 Kio |
| precision | 1 024 | 0,72 GMAC/s | 1,57 GMAC/s | 11,6 ms | 64 Kio / 128 Kio / +20 Kio |
| precision-longue | 4 096 | 2,89 GMAC/s | 6,29 GMAC/s | 46,4 ms | 256 Kio / 512 Kio / +20 Kio |
| resolution | 48 | 0,03 GMAC/s | 0,07 GMAC/s | 0,53 ms | 3 Kio / 6 Kio / +20 Kio |

Un cœur Xeon E5-2630 v4 (Shrek, AVX2+FMA, 2,2 GHz) atteint 3,5 GMAC/s f64
mesurés sur ce produit scalaire (§ 6.1 ; l'estimation initiale de ~5 était
optimiste) ; un Raspberry Pi 4 (NEON f64 ×2, 1,5 GHz)
plutôt 0,5 à 1 GMAC/s. D'où le test de la machine (§ 5). Une mise en œuvre
par convolution rapide partitionnée (comme le convolveur de pièce) ramènerait
le coût du « précision » de 1 024 à quelques dizaines d'opérations par
échantillon ; c'est l'optimisation à faire si le polyphase direct ne tient pas
sur les petites machines (question Q4).

## 4. Dither et mise en forme du bruit

Structure par rétroaction d'erreur (Gerzon & Craven 1989) :
`v = x − Σ c_k e[n−k]`, `y = Q(v + d)`, `e = y − v`, d'où
`NTF(z) = 1 − Σ c_k z⁻ᵏ`. Le dither `d` est TPDF ±1 LSB, tiré d'un xorshift64*
à graine explicite : déterministe, comme le dither du serveur (la règle
`depuis_contenu` s'applique telle quelle). L'erreur réinjectée est bornée à
±2 LSB pour qu'un écrêtage n'emballe pas la boucle.

Formeurs du catalogue (`dither.rs::formeurs`) :

| id | ordre | coefficients `c_k` | cadence | référence |
|---|---|---|---|---|
| `ordre1`…`ordre4` | 1 à 4 | binomiaux : NTF = (1 − z⁻¹)ⁿ | toutes | calculés ; Gerzon & Craven 1989 |
| `wannamaker3f` | 3 | 1,623 ; −0,982 ; 0,109 | 44,1 / 48 | Wannamaker 1992, pondération F |
| `lipshitz5e` | 5 | 2,033 ; −2,165 ; 1,959 ; −1,590 ; 0,6149 | 44,1 / 48 | Lipshitz, Wannamaker & Vanderkooy 1991, pondération E |
| `wannamaker9f` | 9 | 2,412 ; −3,370 ; 3,937 ; −4,174 ; 3,353 ; −2,205 ; 1,281 ; −0,569 ; 0,0847 | 44,1 / 48 | Wannamaker 1992, pondération F |
| `wannamaker9e` | 9 | 2,847 ; −4,685 ; 6,214 ; −7,184 ; 6,639 ; −5,032 ; 3,263 ; −1,632 ; 0,4191 | 44,1 / 48 | Wannamaker 1992, pondération E améliorée |

Les formeurs psychoacoustiques publiés sont optimisés pour 44,1 kHz : à
352,8 kHz leur courbe n'a plus de sens. Aux cadences hautes, la bande
20 Hz–20 kHz n'occupe plus que 5 à 6 % de Nyquist et les formeurs binomiaux,
sans hypothèse psychoacoustique, repoussent l'essentiel du bruit dans
l'ultrason : l'ordre 2 gagne environ 25 dB dans la bande audible, l'ordre 4
environ 45 dB (`∫ ω⁴` et `∫ ω⁸` sur la bande). Le banc mesure le bruit total,
le bruit dans 20 Hz–20 kHz et le bruit pondéré par l'inverse du seuil
d'audition (formule de Terhardt 1979). Le théorème de Gerzon-Craven rappelle
le prix : la puissance totale du bruit **monte** de `10 log(1 + Σ c_k²)`
(+9,5 dB pour l'ordre 2, +18,6 dB pour l'ordre 4, +23,6 dB pour `wannamaker9f`) ;
à 16 bits et 352,8 kHz le bruit ultrasonore mis en forme atteint
−70 à −80 dBFS, ce qu'un amplificateur à faible marge n'aime pas forcément
(question Q3).

## 5. Test de la machine

`machine.rs::tester_la_machine(canaux, durée, marge)` construit chaque filtre
pour chacun des six couples de référence, traite `durée` secondes de bruit
stéréo par blocs de 1 024 trames, garde la meilleure de deux passes et rend
le facteur « × temps réel », le débit en GMAC/s, la mémoire des phases et le
chemin SIMD choisi (`avx2+fma`, `neon`, scalaire). Un filtre est **admis**
si le facteur atteint la marge (×3 par défaut : le producteur partage le cœur
avec le décodage, l'égaliseur et le convolveur). Au premier usage, le serveur
lancera ce test hors du fil audio, mémorisera le verdict par couple et
masquera dans l'interface les filtres non admis ; un bouton « remesurer »
relancera le test (changement de machine, de gouverneur CPU).

## 6. Mesures

**Banc passé sur Shrek le 2 novembre 2026** (07 h 55 – 08 h 13, heure de
Paris). Machine : 2 × Xeon E5-2630 v4 (10 cœurs / 20 fils par socle,
2,2 GHz, turbo 3,1 GHz ; L1d 32 Kio, L2 256 Kio par cœur, L3 25 Mio par
socle), AVX2+FMA, Linux, rustc stable, profil `release` (opt 3, LTO thin,
une unité). Machine partagée : passage complet sous une charge montée de 9 à
41 (sur 40 fils), trois passages « débit » sous charge 8 à 12, tous les bancs
sous `nice -n 10`, un fil (huit au plus pour le débit en parallèle). Aucun
son : tout reste en mémoire. Tableaux complets dans le corps de #7644.

Commandes, dans la crate :

```
nice -n 10 cargo build --release -j4 --examples --bins
nice -n 10 target/release/examples/banc_machine <dossier>          # §§ 6.1 à 6.3
nice -n 10 target/release/examples/banc_machine <dossier> --debit  # § 6.1 seul
nice -n 10 target/release/banc <dossier>                           # resume.md, CSV, SVG
nice -n 10 cargo test --release -j4 --no-fail-fast
```

`examples/banc_machine.rs` ne fait que composer l'API publique de la crate.

### 6.1 Débit du produit scalaire polyphase (f64, un fil, GMAC/s)

| chemin | L | 48 | 256 | 1 024 | 2 048 | 4 096 |
|---|---|---|---|---|---|---|
| avx2+fma | ×8 | 2,85 – 3,11 | 5,88 – 6,47 | 3,58 – 3,91 | 4,55 – 4,86 | 2,99 – 3,25 |
| avx2+fma | ×16 | 2,89 – 3,18 | 3,52 – 3,73 | 3,44 – 3,75 | 3,19 – 3,35 | 3,02 – 3,39 |
| scalaire (8 accumulateurs) | ×8 | 1,75 – 2,02 | 2,25 – 2,50 | 2,37 – 2,66 | 2,15 – 2,32 | 2,17 – 2,41 |
| scalaire | ×16 | 1,92 – 2,13 | 2,21 – 2,47 | 2,38 – 2,63 | 2,23 – 2,44 | 2,03 – 2,27 |

Fourchette de trois passages (médiane de 3 × 2 s chacun). AVX2 ≈ 1,5 × le
scalaire. **Valeur retenue pour ce Xeon : 3,5 GMAC/s par cœur** (portées 256
à 2 048), 3,0 à 4 096, 2,3 sans AVX2. La valeur « ~5 GMAC/s » du § 3.3 était
optimiste. Pas de coude de cache net entre 1 024 et 4 096 (−15 % à ×16) ; à
×8 la courbe n'est pas monotone, ce qui désigne l'alignement des phases plus
que la taille (hypothèse non vérifiée : alias de 4 Kio). Huit fils
simultanés : 20 à 41 GMAC/s au total.

### 6.2 Interpolateur complet, stéréo : facteur temps réel et latence

| conception | ×8 (44,1 → 352,8) | ×16 (44,1 → 705,6) | 48 → 384 | 48 → 768 | latence au pic, linéaire / minimale (44,1) |
|---|---|---|---|---|---|
| resolution (48) | 57,5 | 24,5 | 40,3 | 20,9 | 0,54 / 0,085 ms |
| polyvalent (256) | 17,0 | 11,0 | 19,1 | 10,8 | 2,90 / 0,108 ms |
| precision (1 024) | 5,6 | 3,1 | 5,8 | 2,6 | 11,6 / 0,113 ms |
| precision-longue (4 096) | 1,0 | 0,5 | 0,9 | 0,4 | 46,4 / 0,113 ms |

Les variantes en phase minimale coûtent le même prix (même portée). Avec la
marge d'admission ×3 du § 5, ce Xeon admet `resolution` et `polyvalent`
partout, `precision` à ×8 et à la marge à ×16 (le banc d'origine, passé sous
plus forte charge, le refuse à 705,6 et 768 kHz), et **refuse
`precision-longue` en polyphase direct à toutes les cadences** : la
convolution rapide (I7) est nécessaire pour le 4 096 dès ×8 et pour le 1 024
à ×16.

### 6.3 Propriétés des filtres (44,1 kHz, ×8 et ×16)

- **Délai de groupe.** Constant en phase linéaire. En phase minimale,
  0,07 à 0,10 ms en basse fréquence ; excursion de 20 Hz à 20 kHz : 117 µs
  (48), 635 µs (256), 440 µs (1 024), 449 µs (4 096). Sous 10 kHz elle reste
  ≤ 26 µs, à 15 kHz ≤ 85 µs : l'essentiel se loge entre 15 et 20 kHz.
- **Pré-écho** (phase linéaire), étendue à −120 dB sous le pic :
  0,47 ms (48), 2,3 ms (256), 9,6 ms (1 024), 35,5 ms (4 096) ; énergie plus
  d'1 ms avant le pic : −51 dB (256), −32 dB (1 024), −30 dB (4 096). En phase
  minimale, rien au-delà de 0,1 ms avant le pic à mieux que −113 dB.
- **Gain unitaire par phase** tenu partout à mieux que 0,03 ppm
  (2·10⁻⁷ dB), phase minimale comprise.
- **Réponse mesurée contre conçue** : écart ≤ 1,3·10⁻² dB pour polyvalent,
  precision, precision-longue et leurs variantes (erreur de la méthode à la
  crête à 21 kHz). `polyvalent` : −0,087 dB à 20 kHz, −20,5 dB à 21 kHz ;
  `resume.md` : −0,1 dB à 20 015 Hz et 135,7 dB à Nyquist ; `precision`
  146,5 dB, `precision-longue` 145,4 dB au-delà de Nyquist ; `resolution`
  −6 dB à Nyquist. **Pour `resolution`, la mesure à la crête n'est pas
  valable** : l'image passe et s'ajoute ; il manque au banc la mesure du seul
  fondamental.

### 6.4 Bruit de quantification (dBFS ; puissance rapportée à 1, ajouter 3,0 dB en convention AES17)

| cadence | formeur | 16 b, 20 Hz–20 kHz | 16 b, 20 kHz–Fs/2 | 24 b, 20 Hz–20 kHz | 24 b, 20 kHz–Fs/2 |
|---|---|---|---|---|---|
| 44,1 | TPDF seul | −96,8 | −106,5 | −144,9 | −154,7 |
| 44,1 | wannamaker9f | −82,0 | −80,0 | −130,1 | −128,2 |
| 88,2 | TPDF seul | −99,8 | −99,0 | −147,9 | −147,1 |
| 88,2 | ordre2 | −101,7 | −88,7 | −149,8 | −136,9 |
| 176,4 | ordre2 | −116,0 | −88,6 | −164,2 | −136,7 |
| 352,8 | TPDF seul | −105,8 | −96,9 | −154,0 | −145,0 |
| 352,8 | ordre2 | −130,8 | −88,6 | −179,0 | −136,7 |
| 352,8 | ordre4 | −151,4 | −77,9 | −199,7 | −126,0 |
| 768 | ordre4 | −181,6 | −77,9 | −229,7 | −126,1 |

Lecture : aux cadences ≥ 176,4 kHz, l'ordre 2 gagne ~25 dB et l'ordre 4
~45 dB dans la bande audible sur le TPDF, comme prévu au § 4 ; le bruit
ultrasonore reste à −88,6 (ordre 2), −83,3 (ordre 3) et −77,9 dBFS (ordre 4)
à 16 bits, au moins 15 dB sous un plafond de −60 dBFS, et vers −126 dBFS à
24 bits. **À 88,2 et 96 kHz, les binomiaux ne gagnent presque rien** (au
plus 3 dB, l'ordre 4 rien) : à ×2, le TPDF seul suffit. À 44,1 kHz, les
formeurs psychoacoustiques montent le bruit non pondéré (c'est leur
principe) ; leur gain se lit sur la mesure pondérée Terhardt de `resume.md`.

### 6.5 Ce que le banc ne mesure pas encore

La phase intermédiaire (absente du catalogue), la réponse à un échelon,
l'intermodulation 19 + 20 kHz, le gain du seul fondamental pour un filtre qui
laisse passer des images, le chemin scalaire dans l'interpolateur complet
(pas de forçage du chemin), la convolution rapide (absente).

**Tests de la crate** : 3 rouges sur 30 au passage du 2 novembre,
`phase_minimale::module_conserve_et_energie_devant` (écart 1,0002·10⁻⁴ dB
pour un seuil de 10⁻⁴), `gabarits::polyvalent_apodise_avant_nyquist`
(gabarit demandant ≤ 0,01 dB jusqu'à 20 kHz alors que la conception met
−0,1 dB à 20,0 kHz : gabarit contradictoire avec la conception) et
`gabarits::thd_n_numerique_sous_le_plancher_24_bits` (−153,5 dB pour
< −160 dB). Ils sont à trancher à l'entrée de la crate dans le workspace
(I2) : corriger le gabarit contradictoire, ou la conception.

## 7. Intégration proposée

**Position dans la chaîne.** Le suréchantillonneur ne peut pas être un
`Processor` en place : il change la cadence et le nombre de trames. Deux voies :

1. **Étendre le SDK** d'un second trait, `RateChanger` (ou un `Processor`
   dont `prepare` rend un `AudioFormat` de sortie différent et dont `process`
   écrit dans un bloc de sortie séparé). L'hôte (`outputs/local.rs`, plus tard
   OAAT) insère cet étage **après** `apply_local_dsp` (l'égaliseur et le
   convolveur restent à la cadence source, moins chers et déjà réglés pour
   elle) et **à la place** du rééchantillonneur rubato de `convertir` : quand
   le greffon est actif, c'est lui qui porte la cadence vers celle demandée au
   périphérique. La quantification et le dither du puits suivent, avec le
   formeur choisi.
2. Le brancher **directement dans `convertir`** comme alternative à rubato,
   sans passer par le SDK. Plus court, mais il ne serait pas un greffon et ne
   bénéficierait ni du manifeste, ni des réglages, ni de l'entitlement.

L'étude recommande la voie 1 : c'est la seule qui respecte « un greffon DSP
dans la chaîne existante ».

**Négociation de la cadence.** Le greffon annonce la cadence qu'il veut
produire (la plus haute acceptée ≤ réglage « cadence cible » de l'utilisateur) ;
`decide_local_rate_opening` doit alors ouvrir le périphérique à cette cadence
(mesurée acceptée) ou refuser le greffon pour cette piste avec une raison
visible. Pour les sorties réseau, le plafond `max_sample_rate` de la zone fait
office de limite.

**PURE.** `pure_bypass` vrai ⇒ le greffon n'est pas instancié ; s'il se
bascule en vol, l'étage se vide et rubato reprend (ou la cadence source est
rouverte, comme pour l'égaliseur aujourd'hui). Le chemin du signal affiche
`dsp = Applied`, `bit_perfect = false`, raison `Upsampled { de, vers, filtre }`
quand il est actif, et une ligne « Suréchantillonnage inactif (mode PURE) »
quand PURE l'écarte.

**Réglages dans l'interface** (par zone, persistés dans les réglages du
greffon, `config_version = 1`) : activé/désactivé (défaut désactivé) ; filtre
(parmi les admis par le test de la machine) ; cadence cible (`×4`, `×8`,
`×16`, « maximum du périphérique ») ; famille (44,1 ou 48 suivie, ou forcée) ;
dither (TPDF seul, formeur) ; profondeur de sortie (24 bits, 32 bits, 16 bits
pour les sorties qui l'imposent) ; bouton « remesurer la machine ». La
réponse en fréquence du filtre choisi peut être rendue par le même canal que
`UiCommand::FrequencyResponse` de l'égaliseur.

**Déterminisme et cache.** Le dither reste dérivé du contenu du bloc ; le
suréchantillonnage lui-même est déterministe. Le cache de transcodage et la
reprise par `Range` (OAAT) gardent donc leurs contrats si le greffon est un
jour appliqué sur ces chemins.

## 8. Découpage proposé

| étape | contenu | preuve |
|---|---|---|
| **I0** | Cette étude : conceptions, banc, gabarits, test de la machine. Discussion avec le membre de la communauté, arbitrage des questions ouvertes. | PR en brouillon, commentaire sur #7614 |
| **I1** | Inventaire exact des cadences acceptées par chaque sortie (locale par hôte, OAAT, DLNA, OpenHome, Squeezebox, Chromecast) ; décision de la voie d'intégration (trait `RateChanger` du SDK). | note courte, issue par sortie si besoin |
| **I2** | SDK : trait de changement de cadence, négociation du format de sortie, `latency_frames` en trames de sortie, schéma de réglages. Greffon natif `tune-plugin-surechantillonnage` construit sur la crate de l'étude (f64 → f32 d'interface, allocation hors du fil audio). | tests du SDK, testkit |
| **I3** | Hôte local : insertion après `apply_local_dsp`, remplacement de rubato quand actif, ouverture du périphérique à la cadence cible, vidage aux frontières gapless (même logique que `vider`), chemin du signal, PURE. | bancs `outputs/local` existants + témoins nouveaux (`garde_de_site_porteur_dop_3233` compte toujours une seule écriture) |
| **I4** | Dither et formeurs dans le puits de sortie : sélection par cadence, déterminisme `depuis_contenu`, 16/24/32 bits. | gabarits de bruit (niveaux pondérés) |
| **I5** | Test de la machine au premier usage, persistance du verdict, masquage dans l'interface, bouton « remesurer ». | test d'intégration serveur |
| **I6** | Interface web : panneau du greffon par zone, courbe de réponse, affichage dans le chemin du signal. | captures, i18n |
| **I7** | Sorties réseau (OAAT/DLNA/OpenHome) si I1 le justifie ; convolution rapide partitionnée pour les petites machines si le test de la machine l'exige. | banc machine sur RPi |
| **I8** | Plus tard : DSD (modulateur sigma-delta de sortie), hors de cette étude. | — |

## 9. Questions ouvertes pour le membre de la communauté

- **Q1 — Apodisant.** La coupure à 0,468 Fs (−0,1 dB à 20,0 kHz) est-elle le
  bon compromis, ou préférez-vous 0,48 Fs (−0,1 dB à 20,6 kHz, apodisation
  moins franche à 22 kHz) ? La gaussienne vous convient-elle face à une
  Kaiser à lobes maîtrisés (transition plus courte, mais oscillation
  résiduelle lointaine à −120 dB) ?
- **Q2 — Phase minimale.** La variante par cepstre a une phase non linéaire
  sur toute la bande (délai de groupe croissant vers 20 kHz). Souhaitez-vous
  plutôt une phase **intermédiaire** (mélange linéaire/minimale, par exemple
  par pondération du cepstre), et avec quel réglage ?
- **Q3 — Formeurs aux cadences hautes.** Aux cadences ≥ 88,2 kHz, les
  binomiaux d'ordre 2 à 4 suffisent-ils, ou faut-il concevoir un formeur
  optimisé (moindres carrés pondérés par le seuil d'audition, avec contrainte
  sur le bruit ultrasonore total) ? Quel plafond de bruit ultrasonore
  (dBFS) jugez-vous sûr pour les amplificateurs à large bande ?
- **Q4 — Longueur et coût.** Le « précision » à 1 024 échantillons d'entrée
  (430 Hz de transition) vous paraît-il suffisant, ou visez-vous 4 096 voire
  plus ? Si oui, acceptez-vous une mise en œuvre par convolution rapide (même
  réponse, latence de bloc en plus) pour les petites machines ?
- **Q5 — Familles et rapports.** Préférez-vous suivre la famille de la source
  (44,1 → 352,8 ; 48 → 384) ou forcer une cadence unique de sortie avec le
  second étage rationnel ? Ce second étage (sinc court, 150 dB) vous
  satisfait-il ?
- **Q6 — Dither à 24 bits.** TPDF à 24 bits (≈ −141 dBFS) sans formeur
  suffit-il, ou voulez-vous un formeur même à 24 bits pour les sorties 24
  bits à cadence haute ?
- **Q7 — Mesures.** Quelles mesures manquent au banc pour votre jugement
  (réponse à un échelon, pré-écho en dB, délai de groupe en fonction de la
  fréquence, intermodulation 19+20 kHz) ?

### 9 bis. Réponses reçues (2 novembre) et ce que les mesures en font

Le membre de la communauté a répondu aux sept questions par une note de
conception reçue le 2 novembre (résumée dans #7614). Confrontées au banc du
§ 6 :

| question | réponse reçue | après mesure | statut |
|---|---|---|---|
| Q1 | garder 0,468 Fs et la gaussienne | rien ne s'y oppose (§ 6.3) | **close** |
| Q2 | phase intermédiaire fixe, après mesure du délai de groupe | **la mesure ne la justifie pas** : excursion de 117 à 635 µs, concentrée entre 15 et 20 kHz ; catalogue initial en linéaire et minimale seulement, l'intermédiaire reste à décider à l'écoute | **changée**, à confirmer par le membre de la communauté |
| Q3 | binomiaux aux cadences hautes, plafond ultrasonore −60 dBFS | confirmé (≤ −77,9 dBFS à 16 bits, ordre 4) ; **nuance** : à 88,2/96 kHz les binomiaux ne gagnent rien, TPDF seul à ×2 | **close avec nuance** |
| Q4 | 1 024 en Auto, 4 096 et plus en Expert, convolution rapide partout | confirmé et renforcé : 4 096 direct < temps réel sur ce Xeon, 1 024 à la marge à ×16 | **close** ; la convolution rapide devient un préalable du 4 096 (I7) |
| Q5 | suivre la famille de la source | retenu (pas de second étage rationnel par défaut) | **close** |
| Q6 | TPDF seul à 24 bits | confirmé : −145 dBFS (44,1) à −154 dBFS (352,8) dans la bande | **close** |
| Q7 | délai de groupe, pré-écho fenêtré, bruit ultrasonore à 24 bits ; intermodulation en analogique ou pas | les trois premières mesurées (§ 6.3, 6.4) ; échelon et fondamental seul encore absents | **close** pour le catalogue initial |

Prérequis relevés par la même note : ordre volume / dither (audit du
2 novembre : défaut en WASAPI exclusif et ASIO natif, #7703, PR #7705),
continuité DSD (#7704, PR #7707), cadence DSD maximale du périphérique, et
changement de cadence dans le SDK (étape I2, engagée).

## 10. Références

- J. F. Kaiser, « Nonrecursive digital filter design using the I0-sinh window
  function », *Proc. IEEE ISCAS*, 1974.
- F. J. Harris, « On the use of windows for harmonic analysis with the
  discrete Fourier transform », *Proc. IEEE* 66(1), 1978.
- A. V. Oppenheim, R. W. Schafer, *Discrete-Time Signal Processing*, 3ᵉ éd.,
  chap. 7 (fenêtrage) et § 13.6 (phase minimale par le cepstre).
- R. E. Crochiere, L. R. Rabiner, *Multirate Digital Signal Processing*,
  Prentice-Hall, 1983, chap. 3 (structures polyphases).
- P. G. Craven, « Antialias Filters and System Transient Response at High
  Sample Rates », *J. Audio Eng. Soc.* 52(3), 2004 (filtres apodisants).
- M. A. Gerzon, P. G. Craven, « Optimal Noise Shaping and Dither of Digital
  Signals », AES Convention 87, 1989.
- S. P. Lipshitz, R. A. Wannamaker, J. Vanderkooy, « Minimally Audible Noise
  Shaping », *J. Audio Eng. Soc.* 39(11), 1991.
- R. A. Wannamaker, « Psychoacoustically Optimal Noise Shaping »,
  *J. Audio Eng. Soc.* 40(7/8), 1992.
- E. Terhardt, « Calculating virtual pitch », *Hearing Research* 1, 1979
  (formule du seuil d'audition).
- J. O. Smith, *Introduction to Digital Filters with Audio Applications*,
  W3K, 2007 (en ligne), « Creating Minimum Phase Filters ».
