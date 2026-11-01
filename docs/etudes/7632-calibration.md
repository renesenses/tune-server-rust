# Étude #7632 — Calibration de la pièce d'écoute

*Note de conception, 01/11/2026 → 04/11/2026. Fable 5.1 pour Bertrand. Cible : rc11 ou rc12. Branche `etude/7632-calibration-b209`, prototype `tune-calibration/` (ex-`tools/tune-calibration/`) (hors workspace, aucun son joué ni capté).*

Décisions de Bertrand du 01/11 : micro USB étalonné sur le serveur **et/ou** micro du téléphone (appli Android ou iOS, étalonnage par modèle) ; correction d'abord par **égaliseur paramétrique automatique**, puis **FIR en phase mixte** avec le convolueur existant ; découpage C1 (mesure) → C2 (cible + EQ) → C3 (FIR) → C4 (positions multiples, vérification, A/B, profils, REW).

---

## 1. État des lieux dans Tune (résumé ; détail en [annexe A](7632-calibration-annexe-a-etat-des-lieux.md))

- **Convolueur** (`tune-core/src/audio/convolver.rs`) : convolution partitionnée uniforme par FFT (`realfft`, f32), bloc **1024 codé en dur** = latence fixe de 21 ms à 48 kHz ; **aucun rééchantillonnage du filtre** (#5190 fermé « not planned ») : une IR à la mauvaise cadence est retirée en silence ; WAV seulement, pas de limite de taps, pas de bench, allocations à chaque bloc.
- **Égaliseur natif** (`sdk/tune-plugin-equalizer`) : biquads RBJ f64, jusqu'à 256 bandes `{freq, gain, q, type, channel?}`, bornes 10 Hz–0,45 fs / Q 0,1–30 / ±24 dB, `headroom_mode`. **C'est le format de sortie du prototype C2**, via `POST /zones/{id}/eq`.
- **Une couche `room_correction.rs` existe déjà** (profils, auto-EQ naïf qui relance jusqu'à +12 dB, upload d'IR) mais **rien ne mesure**. Défaut probable à vérifier : `apply_profile_handler` écrit un `EqProfile` incomplet que le lecteur ne désérialise pas.
- **Entrée audio** (`plugins/tune-entree-audio`) : capture cpal, formats I16/I24/I32/F32, cadence du périphérique, blocs horodatés, un micro USB y apparaît ; **aucune API ne rend les échantillons à un analyseur** (le PCM n'est servi que comme source de lecture).
- **Analyse acoustique** : aucune (ni RT60, ni balayage). `realfft` est déjà une dépendance de `tune-core` : le prototype n'en ajoute pas.
- **Profils par zone** : clés `settings` séparées (EQ, IR, crossfeed, PURE), pas de profil de correction regroupant EQ + IR.
- **Chemin du signal** : EQ → convolueur → crossfeed → mono ; **PURE court-circuite tout** déjà ; mais `signal_path` n'a **aucune étape « IR/Convolver »** : à ajouter, avec un motif `pure_degraded`.
- **Sorties** : seule la sortie locale (même hôte que le micro) et OAAT (PTS absolu) permettent une synchronie propre ; AirPlay/DLNA/Chromecast passent par un repère de synchronisation. Aucun générateur de signal interne : le mécanisme à employer est un `FournisseurPcm` « balayage ».
- **Applis** : aucune capture micro ni permission dans `tune-remote-flutter` et `tune-remote-ios`.

## 2. Conception

### 2.1 Mesure : balayage de Farina

**Signal.** Balayage sinusoïdal logarithmique (ESS) x(t) = sin(2π·f₁·L·(e^{t/L} − 1)), L = T/ln(f₂/f₁) ; filtre inverse = balayage retourné pondéré par e^{−t/L} (+6 dB/oct en magnitude, compense le spectre rose de l'ESS) ; x ⊛ inv ≈ δ. Les harmoniques d'ordre k du système se replient à t_k = −L·ln k **avant** l'impulsion linéaire : la distorsion des enceintes à fort niveau ne pollue pas la réponse linéaire, à condition que la fenêtre d'extraction commence après t₂ (Farina, AES 108, 2000 ; AES 122, 2007). Réglage par défaut du prototype : 10 Hz → 22 kHz (bornes hors bande utile pour rejeter le ringing des bords), 8 s, −12 dBFS, fondus 200 ms / 10 ms. Le prototype normalise l'inverse numériquement (auto-déconvolution = 1) ; le résidu hors ±2 ms est < −40 dB.

Pourquoi pas MLS ou bruit rose : l'ESS a le meilleur RSB à durée égale, sépare la distorsion, tolère une légère non-stationnarité ; MLS est très sensible à la non-linéarité et à la dérive d'horloge (Stan, Embrechts, Archambeau, « Comparison of different impulse response measurement techniques », JAES 50(4), 2002).

**Déconvolution.** Convolution linéaire par FFT de la capture avec l'inverse (`deconv::deconvolve`) ; pic = première arrivée après le retard nul ; extraction [−50 ms, +1,5 s] ; le retard total (périphériques + vol acoustique) tombe au sample près (essai `retard_pur_retrouve_au_sample_pres`, 12 ms = 576 éch. sur pièce simulée).

**Fenêtrage.** Demi-Hann 5 ms avant le pic, plateau, demi-Hann sur la seconde moitié d'une fenêtre de 500 ms (résolution 2 Hz, nécessaire aux modes) ; une fenêtre courte (5–10 ms, « gating ») donne la réponse quasi-anéchoïque de l'enceinte au-dessus de ~300 Hz — même outil, deux réglages. Lissage **1/12 d'octave** pour l'affichage et la correction (REW : 1/6 à 1/12 pour l'EQ, « psychoacoustic » pour l'oreille) ; 1/48 pour le diagnostic.

**Alignement temporel.** Trois cas :
1. **Micro USB sur le serveur + sortie locale** : lecture et capture partagent l'hôte ; le retard apparaît dans la déconvolution et n'a pas besoin d'être connu a priori. Une **boucle de référence** électrique (sortie → entrée) n'est pas nécessaire pour la magnitude ; elle le devient en C3 si l'on veut la phase absolue et l'alignement temporel gauche/droite au sample près (REW « timing reference »). Alternative sans câble : l'**acoustic timing reference** de REW (un repère joué sur un canal fixe avant chaque balayage).
2. **Micro du téléphone** ou sortie réseau : pas d'horloge commune. Le prototype place un **repère de synchronisation** (salve pseudo-aléatoire de 100 ms, autocorrélation étroite) avant le balayage ; l'intercorrélation retrouve t₀ au sample près (essai `le_repere_de_synchronisation_retrouve_l_origine_sans_horloge_commune`, origine inconnue de 31 337 éch. retrouvée). Deux repères (début et fin) donnent la **dérive d'horloge** en ppm (`deconv::clock_drift_ppm`) ; au-delà de ~50 ppm sur 10 s (0,5 ms d'étalement), il faut rééchantillonner la capture avant déconvolution (rubato est déjà vendorisé) — Farina 2007 montre qu'une dérive non compensée étale l'impulsion et creuse les aigus.
3. **OAAT** : le PTS du paquet donne t₀ ; on ne mesure alors que DAC + acoustique.

**Étalonnage du micro.** Fichier texte miniDSP (UMIK-1/UMIK-2) ou REW `.cal` : en-tête `"Sens Factor =−1.021dB, SERNO: …"`, puis lignes `fréquence  écart_dB  [phase]` ; commentaires `*`/`#`. Correction = mesure − écart (interpolation en log f, bornée hors plage). Lu par `mic_cal::MicCalibration::parse` (essais sur les deux variantes). Fichiers 0° (micro vers l'enceinte) et 90° (micro vers le plafond, recommandé pour la moyenne spatiale) : l'utilisateur choisit, l'UI le rappelle. **Convention SPL à confirmer** sur un micro réel : UMIK-1 nominal −18 dBFS à 94 dB SPL, `SPL = dBFS + 112 − SensFactor` (question au relecteur).

**Niveau et plafond de pression.** `level::LevelPolicy` : salve d'essai à −30 dBFS, montée par pas de 6 dB tant que la crête captée < −6 dBFS, le niveau joué < −6 dBFS, le SPL estimé < 90 dB (si le micro porte un facteur de sensibilité), et jusqu'au RSB visé (50 dB sur l'IR) ; refus explicite avec motif sinon. Au moment de la mesure : volume de la zone **forcé à une valeur connue** et restauré ensuite, convolueur et EQ **désactivés** sur la zone (mesure de la pièce nue), affichage d'un avertissement « éloignez-vous des enceintes, protégez vos oreilles ». Un balayage à 85 dB SPL pendant 8 s est bénin (norme ISO 1999 : 8 h à 85 dB) ; le danger est la fausse manœuvre (volume à fond) : d'où la montée progressive et le plafond de niveau joué, jamais une seule salve pleine échelle.

### 2.2 Plusieurs positions

- **Positions fixes** (REW, Dirac) : 3 à 9 points autour de la position d'écoute (tête, ±30 cm, ±60 cm, hauteur ±20 cm), pondérées (centre 2, autres 1). Chaque position garde sa mesure (C4 : profils par position).
- **MMM** (Moving Microphone Method, Jean-Luc Ohl, 2013 ; Hendrix/Ohl, « MMM », 2016 ; repris par Dirac, Trinnov) : bruit rose ou balayages répétés pendant que le micro décrit lentement un volume de ~1 m³ ; moyenne énergétique de spectres courts (RTA, 1/12 oct). Plus rapide et plus représentatif pour la **magnitude**, mais **sans réponse impulsionnelle** (ni RT60, ni cascade, ni phase, ni FIR). Décision proposée : positions fixes en C1/C4, MMM en option C4 **pour la magnitude seulement** (le C1 fournit déjà le RTA par tranches).
- **Moyenne** : puissance (RMS) par défaut comme REW (`analysis::spatial_average(power = true)`), dB en option. Les creux non communs (nœuds d'une position) s'effacent, les modes communs restent : l'essai `la_moyenne_spatiale_lisse_les_differences_entre_positions` vérifie que le mode partagé (42 Hz) est coupé d'au moins 2 dB de plus que le mode discordant (68 Hz, creux en position 2).
- L'écart gauche/droite (`difference_db`) est montré, et l'EQ peut être **par canal** (`channel: 0|1` du format natif) : à proposer seulement quand l'écart dépasse ~3 dB sur une demi-octave dans les graves, sinon un EQ commun (l'oreille tolère mieux une asymétrie basse fréquence qu'un déséquilibre tonal).

### 2.3 Analyse

- Lissage 1/N octave en dB (fenêtre [f·2^{−1/2N}, f·2^{1/2N}]), grille log 48 pts/oct de 20 Hz à 0,45·fs.
- **RT60** par octave (31,5 Hz – 16 kHz) : passe-bande à phase nulle (prototype ; serveur : Butterworth ordre 6 CEI 61260), **intégration rétrograde de Schroeder** (JASA 1965) avec soustraction du bruit estimé sur les derniers 10 % (Chu 1978), régressions EDT (0/−10 dB ×6), T20 (−5/−25 ×3), T30 (−5/−35 ×2) selon ISO 3382-1 ; dynamique disponible affichée, `NaN` si elle manque. Une pièce domestique correctement amortie : 0,3–0,5 s au-dessus de 200 Hz ; > 0,8 s dans les graves signale des modes mal amortis que l'EQ ne « répare » qu'à la position mesurée.
- **Cascade** (CSD) : fenêtres glissantes 300 ms / pas 30 ms, 10 tranches, lissage 1/6, dB relatifs au maximum initial ; c'est ce qui montre les modes qui *traînent*.
- Écart gauche/droite point à point ; moyenne spatiale ; export texte REW de chaque courbe.

### 2.4 Cible et égaliseur paramétrique automatique (C2)

**Cible.** `TargetCurve::Tilt { dB/oct, pivot }` (plate par défaut ; −0,5 à −1 dB/oct recommandé en pièce : Toole, *Sound Reproduction*, 3e éd., 2017, ch. 12 ; Olive, Welti, McMullin, « Listener preference for in-room loudspeaker response », AES 135, 2013) ou **courbe de l'utilisateur** (`Points`, p. ex. Harman in-room fournie). La cible est alignée sur la mesure par la **médiane** des écarts dans la bande d'alignement (40–300 Hz en un point) : robuste aux pics, elle passe « au milieu » de la mesure ; une moyenne serait tirée vers le haut par les modes et l'égaliseur, qui ne coupe que, sous-corrigerait (défaut vu et corrigé pendant le prototype).

**Zone de correction bornée.** Par défaut **20–300 Hz en un point**, 20–500 Hz en moyenne spatiale ; au-dessus de la fréquence de Schroeder (f_S ≈ 2000·√(RT60/V) ≈ 150–300 Hz dans un salon), la réponse en un point est dominée par des interférences locales que l'EQ dégraderait ailleurs (Toole ch. 8 ; Welti & Devantier, « Low-frequency optimization using multiple subwoofers », JAES 54(5), 2006). Au-dessus, seule la cible tonale large (shelves, pente) a du sens ; le prototype les expose (`BandType::LowShelf/HighShelf`) mais l'ajustement automatique ne les pose pas en C2.

**Algorithme** (`eq::fit`, inspiré de REW « Match response to target » et d'AutoEq) :
1. erreur e(f) = mesure − (cible + offset) sur la grille log de la zone ;
2. glouton : pic positif le plus haut au-dessus du seuil (1 dB) → bande `peak` à f₀, gain = −e, Q = f₀/largeur à mi-hauteur ; **jamais de relance** (`max_boost_db = 0` par défaut : un creux est presque toujours une annulation, un gain y brûle l'ampli et le haut-parleur sans effet : Toole § 8.2) ;
3. raffinement par descente de coordonnées (f ×2^{±1/6}, Q ×1,3^{±1}, gain ±1 dB, pas décroissants) sur un coût RMS **asymétrique** (résidu sous la cible pondéré ×3 : sur-corriger est pire que sous-corriger) ;
4. arrêt à `max_bands` (8 en un point, 10 en moyenne) ou quand le gain marginal < 0,02 dB ;
5. **plafond cumulé** : la somme des coupes ne dépasse jamais `max_cut_db` (12 dB ; deux bandes empilées n'y échappent pas), f ∈ [f_lo, f_hi], Q ∈ [0,5 ; 10] ;
6. marge d'écrêtage : gain cumulé maximal calculé sur 10 Hz–0,45 fs ; préampli = −(max + 0,5 dB) si > 0, sinon 0 ; le serveur reçoit `headroom_mode: "safe"` et l'EQ natif y ajoute sa propre réserve L1.

Sortie : `EqSettings { enabled: true, bands, headroom_mode: "safe" }` sérialisé exactement comme `POST /zones/{id}/eq`, plus l'export REW « Filter Settings file » (`eq::to_rew_filters`).

### 2.5 FIR en phase mixte (C3)

- **Pourquoi mixte** : les modes de pièce et les filtres de l'enceinte sont à phase minimale ; les corriger en phase minimale n'ajoute **aucun pré-écho** et une latence nulle hors bloc. La partie « linéaire » n'apporte rien à la magnitude et coûte des taps et de la latence ; elle n'a de sens que pour aligner les voies ou les enceintes entre elles (excès de phase), ce qui demande une référence de temps (boucle ou repère). Conception : correction en **phase minimale sous ~300–500 Hz** (magnitude de l'EQ C2, ou inverse régularisé de la mesure lissée), identité au-dessus, fondu d'une demi-octave ; option C3-bis « excès de phase » linéaire pour l'alignement G/D.
- **Méthode** : magnitude cible → cepstre réel → repliement → exponentielle (Oppenheim & Schafer § 13.5 ; `scipy.signal.minimum_phase`), prototype `fir::minimum_phase_from_magnitude` ; FIR tronqué avec fondu (`fir::mixed_phase_design`). Essais : la magnitude du FIR suit l'EQ à ±0,3 dB (65 536 points) et ±0,5 dB (16 384 taps, pièce sèche) ; 99 % de l'énergie dans les 4 096 premiers taps.
- **Longueur** : une coupe de Q = 6 à 42 Hz a une constante de temps Q/(πf) ≈ 45 ms ; −60 dB en ~0,3 s → 16 384 taps à 48 kHz suffisent (0,34 s), 32 768 à 96 kHz, 65 536 à 192 kHz. Une inversion directe de la mesure (pas seulement de l'EQ) demande en plus une **régularisation** (Kirkeby & Nelson, « Digital filter design for inversion problems in sound reproduction », JAES 47(7), 1999) pour ne pas remonter les creux : on garde la règle C2 (coupes seulement) en bornant le gain du FIR à 0 dB.
- **Coût** dans le convolueur actuel (bloc 1024, FFT 2048, par canal) : voir le tableau § 3.3 ; 16 384 taps à 48 kHz ≈ 16 partitions, ~30 MFLOP/s par canal : négligeable sur un x86, confortable sur un RPi 4. **Latence** : 1024 trames fixes (21 ms à 48 k) ; acceptable en lecture, **à exclure** pour l'écran synchro vidéo et pour le karaoké. Point dur : **pas de rééchantillonnage** (#5190 fermé) — C3 doit **générer un FIR par cadence** (44,1/48/88,2/96/176,4/192) au moment du calcul (la conception travaille en fréquence, c'est gratuit), ou rouvrir #5190. Recommandation : générer par cadence, fichiers `zone_{id}_{sr}.wav`, et faire choisir `chemin_ir` par `dec_sr`.

### 2.6 Micro du téléphone (option décidée)

**Capture.**
- *Android (Flutter)* : paquet `record` (BSD-3) ou `flutter_sound` (MPL-2.0 — à éviter) ; sous le capot `AudioRecord` avec `UNPROCESSED` (API 24+, si `PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED`) sinon `VOICE_RECOGNITION` ; **impératif** : désactiver AGC, suppresseur de bruit et annuleur d'écho (`AutomaticGainControl`, `NoiseSuppressor`, `AcousticEchoCanceler`), sinon la mesure est inutilisable. Permission `RECORD_AUDIO`. Cadence 48 kHz, mono, 16 bits PCM (le float n'est pas garanti).
- *iOS* : `AVAudioEngine` + `AVAudioSession` catégorie `.playAndRecord` ou `.record`, mode **`.measurement`** (désactive le traitement système), `setPreferredSampleRate(48000)`, `setPreferredIOBufferDuration`. `NSMicrophoneUsageDescription` à ajouter. Le mode `.measurement` est l'atout d'iOS : réponse reproductible d'un modèle à l'autre.
- Le téléphone **ne joue pas** le balayage (il n'est pas la zone) : il capte pendant que le serveur joue par la zone ; il envoie le PCM au serveur (WebSocket binaire ou `POST` multipart d'un WAV après coup — **après coup** est plus simple et plus robuste) ; la déconvolution reste côté serveur.

**Synchronisation.** Sans horloge commune : repère de synchronisation (§ 2.1, cas 2) et mesure de dérive par deux repères. Horloges des téléphones : ±20 à ±50 ppm courants ; sur un balayage de 8 s, 50 ppm = 0,4 ms d'étirement, tolérable pour la magnitude < 1 kHz, à compenser par rééchantillonnage pour la phase ou pour les aigus. Le PCM est horodaté par l'appli au démarrage et à l'arrêt (`AudioRecord.getTimestamp`, `AVAudioTime`) pour vérifier la continuité (pertes de tampon = mesure rejetée).

**Étalonnage par modèle.** Les micros MEMS des téléphones sont **plats à ±2 dB de 100 Hz à ~8 kHz** mais : filtre passe-haut à 50–100 Hz sur beaucoup de modèles (pente 6–12 dB/oct), remontée 5–10 kHz, résonance du port au-delà, **compression/AGC si le mode mesure n'est pas obtenu**, et une dispersion de ±3 dB entre exemplaires du même modèle (études de la communauté REW/AudioTools ; Dirac Live et Sonos Trueplay s'y limitent à l'iPhone pour cette raison, Trueplay refuse Android jusqu'en 2023). Approche : une table d'étalonnage par modèle (`Build.MODEL`, `utsname.machine`), **mesurée par nous ou la communauté contre un UMIK-1** (procédure : téléphone et UMIK côte à côte, même balayage, écart lissé 1/3 oct), format identique aux fichiers `.cal` ; modèle inconnu ⇒ courbe générique et **zone de correction réduite à 40–300 Hz avec coupe max 6 dB**.

**Limites dans les graves.** C'est là que la correction de pièce travaille, et c'est là que le MEMS est le moins fiable : passe-haut interne, bruit propre élevé (RSB à 30 Hz souvent < 30 dB pour un balayage à 80 dB SPL), saturation précoce (~110–120 dB SPL mais compression dès 100 dB sur certains modèles). Un passe-haut à 80 Hz non compensé transforme un pic réel à 40 Hz en bosse plate et l'EQ **sous-corrige** ; un étalonnage par modèle le rattrape **jusqu'à ~30 Hz**, pas au-dessous. Les positions multiples atténuent l'erreur d'exemplaire (elle est commune à toutes les positions, donc elle bouge la cible, pas les coupes relatives).

**Comparaison franche.**

| Critère | Micro USB étalonné (UMIK-1, ~100 €) | Micro du téléphone |
|---|---|---|
| Précision de magnitude 20–300 Hz | ±1 dB avec son fichier | ±3 dB avec étalonnage par modèle, inconnue sans |
| En dessous de 30 Hz | fiable | non fiable |
| Niveau absolu (SPL) | oui (Sens Factor) | non (±5 dB) |
| Synchronisation | triviale (même hôte) | repère + dérive |
| Chaîne logicielle | capture cpal déjà là, à exposer | 2 applis à modifier (permissions, mode mesure, envoi), dépend du modèle |
| Expérience utilisateur | câble USB au serveur (souvent dans un placard), positionnement au pied | immédiat, le téléphone est déjà en main ; MMM naturel |
| RT60 / cascade / FIR | oui | magnitude surtout ; RT60 indicatif ; FIR déconseillé |
| Recommandation | **référence** ; C1 d'abord sur lui | **C1-bis** après C1 : mode « rapide » limité à 40–300 Hz, coupes ≤ 6 dB, résultat marqué « estimé » ; le FIR (C3) exige l'USB |

### 2.7 Vérification, A/B, profils, REW

- **Vérification** : après application, même balayage par la zone **avec** la correction active, affichage avant/après et écart à la cible (le prototype le fait : `sim::apply_bands` puis seconde mesure).
- **Bascule A/B** : `enabled` de l'EQ et `ir_path` de la zone, avec la rampe existante (`RAMPE_DE_BASCULE_MS` = 200) ; A/B à volume égal exige de compenser le préampli (coupes seules ⇒ gain moyen légèrement inférieur : on compare à niveau RMS identique, en publiant l'écart).
- **Profils** : par zone, et par position d'écoute (« canapé », « bureau ») : `correction_profile_{zone}` → `{ name, positions: [...], measurements: [...], target, bands, fir: { per_rate: {...} }, created_at }`, remplaçant progressivement `room_profile_{zone}`. Activation = écrit `zone_{id}_eq_profile` (format `EqProfile` complet) et `ir_path_{zone}`.
- **REW** : import des mesures texte (`rew::import_measurement_txt`) et des IR WAV (`rew::wav_read` ; côté serveur `read_wav_ir`), export des mesures texte (`export_measurement_txt`), des IR WAV float32, des filtres « Filter Settings file » (`to_rew_filters`) ; import des filtres REW (format texte « Filter n: ON PK Fc … ») à ajouter en C4 (symétrique, trivial) ; l'export FIR est le WAV existant.

---

## 3. Prototype : résultats sur pièces simulées (résumé ; tableaux en [annexe B](7632-calibration-annexe-b-resultats.md))

Crate `tune-calibration/` (ex-`tools/tune-calibration/`) (Rust 2021, `realfft`, `rustfft`, `serde`, `serde_json`, toutes MIT/Apache-2.0, `cargo deny check licenses` vert) ; 33 essais unitaires + 9 essais d'intégration, verts sur Shrek. Pièces simulées : son direct retardé de 12 ms, réflexion de sol, trois modes (42 Hz +9 dB Q6, 68 Hz +6 dB Q5, 115 Hz +5 dB Q7), bruit −80 dBFS ; variante avec queue diffuse par octave à partir de 125 Hz. **Aucun son n'est joué ni capté.**

- **Mesure** : retard retrouvé au sample près (576 éch.), crête/bruit 72 dB, réponse à < 0,3 dB de la réponse vraie de 30 Hz à 10 kHz, les trois modes retrouvés comme maxima ; RT60 T30 à ±10 % de 500 Hz à 4 kHz (±13 % à 125–250 Hz, flancs des filtres d'octave) ; étalonnage micro annulé à 0,25 dB ; origine retrouvée exactement par repère de synchronisation ; analyse complète en 0,4–0,5 s.
- **Égaliseur** (pièce sèche) : 3 bandes à 42,1 / 68,3 / 114,8 Hz (fréquences à 0,3 %), −8,1 / −5,5 / −5,0 dB ; mesure de vérification après correction : +7,07 → −0,01 dB à 42 Hz, +4,98 → −0,03 à 68 Hz, +4,61 → +0,21 à 115 Hz, creux inchangés, aucune relance, préampli 0. Pièce réverbérante : les modes à 42 et 68 Hz corrigés, un pic de champ diffus pris pour cible à 102 Hz et un résidu de 5 dB à 115 Hz signalé par avertissement : c'est l'argument pour la moyenne spatiale (C4).
- **FIR** : 16 384 taps phase mixte conçus en 9 ms, écart à l'égaliseur ≤ 0,03 dB ; coût estimé 17 MFLOP/s par canal à 48 kHz, 141 à 192 kHz / 65 536 taps.
- **Quatre défauts trouvés et corrigés par les essais** : normalisation de l'inverse (+1 dB), fenêtre gauche trop courte (0,4 dB), cible alignée sur la moyenne (sous-correction), réponse de l'égaliseur comparée non lissée (Q trop élevés).

### 3.4 Contre-épreuve

Témoin : `les_garde_fous_de_l_egaliseur_tiennent` (pic de +18 dB, plafond de coupe à 8 dB). Sabotage : `Problem::feasible` remplacé par `true` (le plafond cumulé n'est plus vérifié, tout compile). Commande : `cargo test --release --test pieces_simulees les_garde_fous`. Résultat : **rouge**, `panicked at tests/pieces_simulees.rs:188:5: coupe cumulée -18.75 dB` — deux bandes empilées à −8 dB chacune contournaient le plafond d'une seule. Restauration par `cp` de la sauvegarde, relance : vert (`1 passed`). Le témoin sait rougir et nomme le défaut.

---

## 4. Découpage et estimation

Estimations en jours de travail d'un agent (Codex ou Claude) relu par Bertrand, hors essais sur matériel réel qui sont à lui (micro UMIK-1, pièce, téléphone). Les étapes serveur visent des PR unitaires vers le lot du moment ; rien de ce qui suit n'entre dans rc10.

### C1 — Mesure (rc11) : ~9 j

| Côté | Travail | Estim. |
|---|---|---|
| Serveur | Nouveau module `tune-core/src/audio/calibration/` repris du prototype (sweep, deconv, analysis, mic_cal, level) sans nouvelle dépendance ; essais sur pièces simulées repris tels quels. | 2 j |
| Serveur | Fournisseur PCM « balayage » (`FournisseurPcm` fini : repère + silence + balayage + silence), joué vers la zone choisie par l'orchestrateur ; volume de zone forcé/restauré ; EQ, IR, crossfeed suspendus pendant la mesure ; refus en PURE (ou sortie de PURE confirmée). | 1,5 j |
| Serveur | Capture : consommateur « vers tampon » du `FluxDirect` d'`entree-audio` (micro USB), horodaté ; routes `/api/v1/calibration/{zone}/microphones`, `/measure` (POST : micro, position, niveau), `/measurements/{id}` (IR WAV, réponse texte, RT60, cascade JSON), `/microphones/calibration` (upload `.txt/.cal`). Garde-fous de niveau (`LevelPolicy`) avec salve d'essai. | 2 j |
| Serveur | Étape « Correction de pièce » dans `signal_path` + motif `pure_degraded` ; stockage `calibration_measurement_{id}` (IR dans `$TUNE_DATA_DIR/calibration/`). | 0,5 j |
| Web | Écran « Calibration » dans les réglages de zone : choix du micro et du fichier d'étalonnage, bouton Mesurer avec compte à rebours et avertissement, courbe de réponse (réutiliser `FirResponseCurve.svelte`), lissage, RT60 par octave, cascade (canvas), écart G/D, export REW. | 2,5 j |
| Applis | Rien en C1 (micro du téléphone en C1-bis). | — |
| Preuve réelle | Mesure d'une vraie pièce avec UMIK-1 comparée à REW (même position, même balayage) : écart < 1 dB 20 Hz–10 kHz, RT60 ±10 %. **Sans cette preuve, C1 n'est pas livré.** | Bertrand |

### C1-bis — Micro du téléphone (rc12) : ~7 j

Android (Flutter `record`, `UNPROCESSED`, AGC/NS/AEC coupés, 48 kHz, envoi WAV après coup) 2 j ; iOS (`AVAudioEngine`, session `.measurement`, `NSMicrophoneUsageDescription`) 2 j ; serveur : route de dépôt de capture + repère de synchronisation + dérive + table d'étalonnage par modèle (format `.cal`, modèle inconnu ⇒ courbe générique et bornes serrées) 2 j ; UI 1 j. Preuve : iPhone et un Android courants contre UMIK-1 côte à côte.

### C2 — Cible et égaliseur automatique (rc11 ou rc12) : ~5 j

| Côté | Travail | Estim. |
|---|---|---|
| Serveur | Module `target` + `eq::fit` du prototype ; route `/calibration/{zone}/design` (POST : mesures, cible, bornes) → bandes au format natif + métriques ; application via le chemin existant `POST /zones/{id}/eq` (écriture d'un `EqProfile` **complet**) ; **corriger le JSON incomplet écrit par `apply_profile_handler` et `dac_calibration` si le défaut est confirmé**. | 2 j |
| Serveur | Profil de correction `correction_profile_{zone}` (mesures, cible, bandes, horodatage) ; vérification (re-mesure avec EQ actif) ; A/B (bascule `enabled`, rampe existante). | 1 j |
| Web | Choix de la cible (plate, pente, Harman, fichier), bornes, max bandes/coupe ; avant/après ; liste des bandes éditable (réutiliser l'éditeur expert de l'EQ) ; bouton Appliquer / A/B / Vérifier. | 2 j |
| Preuve réelle | Avant/après sur la vraie pièce, avec REW en juge : les pics 20–300 Hz ramenés à ±2 dB de la cible, aucune relance, préampli 0. | Bertrand |

### C3 — FIR phase mixte (rc12 ou après) : ~6 j

Conception du FIR (`fir::mixed_phase_design` + régularisation Kirkeby si inversion de la mesure) 1,5 j ; **génération par cadence** (44,1 → 192 kHz) et sélection par `dec_sr` dans `chemin_ir` (ou réouverture de #5190) 1,5 j ; écriture WAV float32 par zone/cadence, activation via `ir_path` 0,5 j ; étape `signal_path` avec taps et latence, et refus propre là où 21 ms de latence gênent (karaoké, vidéo) 0,5 j ; correction de `process_block` (allocations par bloc) et de l'enchaînement `process_pcm` sur `StreamingDsp` si confirmé 1 j ; bench du convolueur (RPi 4 / x86) 0,5 j ; UI : courbe FIR vs EQ, choix EQ seul / EQ + FIR 0,5 j. Preuve : mesure de vérification avec FIR actif, pas de pré-écho visible dans l'IR mesurée.

### C4 — Positions multiples, profils, REW (rc12 ou après) : ~6 j

Positions fixes pondérées et moyenne spatiale (prototype prêt) 1 j ; MMM optionnel (RTA par tranches, magnitude seule) 1,5 j ; profils par position d'écoute et bascule 1 j ; import des filtres REW (texte) et des mesures (texte/WAV), export complet 1 j ; EQ par canal si écart G/D > seuil 0,5 j ; UI 1 j.

**Total** : C1 9 j + C1-bis 7 j + C2 5 j + C3 6 j + C4 6 j ≈ **33 jours d'agent**, plus les campagnes de mesure réelles de Bertrand (une demi-journée par étape). Chemin recommandé pour la rc11 : **C1 + C2 sur micro USB** (14 j) ; rc12 : C1-bis, C3, C4.

---

## 5. Risques

1. **Pas de capture vers un analyseur** aujourd'hui : il faut ouvrir `entree-audio` à un second consommateur. Risque de régression sur la lecture en direct ; parade : consommateur séparé sur le même anneau, essais existants d'`entree-audio` conservés.
2. **Cadence du micro ≠ cadence de la zone** (UMIK-1 à 48 kHz, piste 44,1 ou 96 kHz) : la déconvolution exige la même cadence. Parade : la mesure impose la cadence de la zone à 48 kHz pendant la mesure (le balayage est un fournisseur PCM à 48 kHz, la sortie suit), ou rééchantillonne la capture (rubato vendorisé).
3. **Latence et gigue de démarrage des sorties réseau** : le repère de synchronisation couvre le décalage ; la dérive d'horloge ne l'est qu'en mesurant deux repères. AirPlay 1 sans NTP et Chromecast sont les moins sûrs ; **C1 se limite à la sortie locale et OAAT**, les autres sont « meilleur effort » avec avertissement.
4. **Pas de rééchantillonnage de FIR** (#5190 fermé) : un seul WAV ne sert qu'une cadence ; parade C3 : génération par cadence. À défaut, l'utilisateur verrait sa correction disparaître en silence au changement de piste — inacceptable, d'où l'étape `signal_path`.
5. **Sécurité auditive et matérielle** : montée progressive, plafonds, volume forcé/restauré, avertissement, bouton d'arrêt qui coupe le fournisseur PCM. Un balayage à pleine échelle dans un caisson sans filtre peut l'endommager : le plafond joué à −6 dBFS et la limite à 10 Hz sont **non négociables** ; en C1 on refuse de mesurer si le volume de zone est inconnu.
6. **Correction en un point au-dessus de la fréquence de Schroeder** : dégrade ailleurs ; bornes par défaut 300 Hz (un point) / 500 Hz (moyenne), et l'UI explique pourquoi. Jamais de relance par défaut.
7. **Micro du téléphone** : AGC/NS non désactivables sur certains Android (mode `UNPROCESSED` absent) ⇒ mesure fausse en silence. Parade : test de linéarité (deux salves à −30 et −24 dBFS doivent différer de 6 dB ± 1) avant toute mesure, refus sinon ; résultats marqués « estimés ».
8. **Format `EqProfile` incomplet écrit par les routes existantes** (§ 1.3) : si confirmé, C2 hériterait d'un bug silencieux. À vérifier dès le début de C2, correction dans le lot courant.
9. **Charge CPU** : négligeable pour l'EQ ; FIR 16 k taps ≈ 17 MFLOP/s/canal à 48 kHz, mais `process_block` alloue à chaque bloc : à corriger avant d'activer le FIR par défaut sur RPi.
10. **Licences** : prototype MIT/Apache seulement ; `record` (Flutter) BSD-3 ; éviter `flutter_sound` (MPL) et toute bibliothèque de mesure GPL (ex. `python-acoustics`, `pyroomacoustics` pour les essais : non embarquées).

---

## 6. Questions pour un relecteur spécialiste du traitement du signal

1. **Normalisation de l'inverse** : nous normalisons numériquement (auto-déconvolution = 1) plutôt que par la formule analytique de Farina ; le résidu hors ±2 ms est à −40 dB avec des fondus de 200/10 ms. Recommandez-vous un fenêtrage de l'inverse (Farina 2007, § 3) ou l'« inverse régularisé » spectral (Kirkeby) pour abaisser ce plancher, sachant qu'il limite le RSB affiché à ~40 dB ?
2. **Fenêtre d'analyse** : 5 ms / 500 ms demi-Hann pour les modes ; REW emploie par défaut 125 ms à gauche et 500 ms à droite (Tukey 0,25). Y a-t-il un argument pour une fenêtre gauche longue hors mesure de phase absolue ?
3. **Lissage pour l'EQ** : 1/6 d'octave fixe ; le lissage « psychoacoustique » (variable, 1/3 à 1/48) ou ERB serait-il préférable pour 20–500 Hz, et avec quel coût de complexité ?
4. **Cible et alignement** : médiane des écarts sur 40–300 Hz. Préféreriez-vous un ajustement du niveau cible qui minimise la coupe totale (programme linéaire) ou un alignement sur 200 Hz–1 kHz (zone « enceinte ») ?
5. **Coût asymétrique** (résidu sous la cible ×3) et **plafond cumulé** 12 dB : ces valeurs sortent de la pratique (REW ≤ 12 dB de coupe, Dirac ~10 dB). Un critère perceptif (pondération par le masquage temporel des modes, Fazenda et al. 2012 sur l'audibilité du temps de décroissance) changerait-il le choix des bandes ?
6. **Phase mixte** : nous corrigeons en phase minimale sous 300–500 Hz et laissons l'identité au-dessus. Voyez-vous un intérêt à une correction d'excès de phase (linéaire) pour une écoute stéréo domestique, et à quel prix de pré-écho ?
7. **Longueur du FIR** : 16 384 taps à 48 kHz (0,34 s) pour des coupes jusqu'à Q = 10 à 30 Hz. Faut-il plus, ou un filtre **IIR** (les mêmes biquads) rend-il le FIR inutile tant qu'on ne corrige pas l'excès de phase ?
8. **Schroeder sous 100 Hz** : l'estimation de T30 par bande d'octave à 31,5 et 63 Hz souffre de la largeur de bande (filtre d'octave de 22–44 Hz = 3 périodes à 30 Hz). Quelle méthode préférez-vous pour les graves : décroissance modale par ajustement de sinusoïdes amorties, ou simple affichage de la cascade ?
9. **Micro du téléphone** : connaissez-vous une base publique d'étalonnages de MEMS par modèle, ou une procédure de calage sur un point (bruit rose à 1 kHz contre un UMIK) qui vaille mieux qu'une courbe générique ?
10. **SPL absolu** : la convention retenue pour le « Sens Factor » de l'UMIK-1 (`SPL = dBFS + 112 − SensFactor`) est-elle la bonne ? Nous n'avons pas de micro sous la main pour la vérifier.

---

## Références

- A. Farina, « Simultaneous measurement of impulse response and distortion with a swept-sine technique », AES 108th Convention, 2000 ; « Advancements in impulse response measurements by sine sweeps », AES 122nd Convention, 2007.
- G.-B. Stan, J.-J. Embrechts, D. Archambeau, « Comparison of different impulse response measurement techniques », JAES 50(4), 2002.
- M. R. Schroeder, « New Method of Measuring Reverberation Time », JASA 37, 1965 ; ISO 3382-1:2009, *Mesurage des paramètres acoustiques des salles — Partie 1* ; W. T. Chu, « Comparison of reverberation measurements using Schroeder's impulse method and decay-curve averaging method », JASA 63, 1978.
- R. Bristow-Johnson, *Cookbook formulae for audio EQ biquad filter coefficients* (W3C Audio EQ Cookbook).
- F. E. Toole, *Sound Reproduction: The Acoustics and Psychoacoustics of Loudspeakers and Rooms*, 3e éd., Routledge, 2017 (ch. 8 modes et correction, ch. 12 cibles).
- S. Olive, T. Welti, E. McMullin, « Listener Preference for In-Room Loudspeaker and Headphone Target Responses », AES 135th Convention, 2013.
- T. Welti, A. Devantier, « Low-Frequency Optimization Using Multiple Subwoofers », JAES 54(5), 2006.
- O. Kirkeby, P. A. Nelson, « Digital Filter Design for Inversion Problems in Sound Reproduction », JAES 47(7), 1999.
- A. V. Oppenheim, R. W. Schafer, *Discrete-Time Signal Processing*, 3e éd., § 13.5 (phase minimale par cepstre) ; `scipy.signal.minimum_phase`.
- J.-L. Ohl, « Moving Microphone Measurement (MMM) », 2013-2016 (documentation communautaire, utilisée par Dirac et Trinnov).
- B. Fazenda, M. Stephenson, A. Goldberg, « Perceptual thresholds for the effects of room modes as a function of modal decay », JASA 137, 2015.
- J. Mulcahy, *Room EQ Wizard* — aide en ligne : formats d'export, « Match response to target », timing reference, fichiers d'étalonnage ; miniDSP, *UMIK-1 calibration files* ; J. Pasanen, *AutoEq* (algorithme d'ajustement paramétrique, MIT).
- Android `AudioRecord` / `MediaRecorder.AudioSource.UNPROCESSED` ; Apple `AVAudioSession.Mode.measurement`.
- Dépôt : `tune-core/src/audio/convolver.rs`, `sdk/tune-plugin-equalizer/src/engine.rs`, `tune-core/src/room_correction.rs`, `plugins/tune-entree-audio/src/systeme.rs`, `tune-server/src/routes/zones/signal_path.rs`, `tune-core/src/outputs/oaat/multiroom.rs` (lus le 01/11/2026).
