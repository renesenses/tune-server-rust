# Étude #7632 — Annexe A : état des lieux détaillé dans Tune

*Relu dans le dépôt le 01/11/2026, base `batch/bugs-rc10-20261030` (`9fd2d8a06`). Renvoi depuis [la note principale](7632-calibration.md), § 1.*

## 1. État des lieux dans Tune

Tout ce qui suit a été relu dans le dépôt le 01/11 (base `batch/bugs-rc10-20261030`, `9fd2d8a06`).

### 1.1 Convolueur — `tune-core/src/audio/convolver.rs`

- `ConvolverConfig { impulse_response: Arc<[Vec<f32>]>, sample_rate }` (l. 23), `build_for(block_size, target_sr, target_channels)` (l. 60) ; `Convolver` (l. 94) : **convolution partitionnée uniforme** par FFT (`realfft`, f32), partitions de `block_size`, ligne à retard fréquentielle circulaire. La doc dit « overlap-save », le code fait de l'overlap-add (remplissage à zéro + report de la queue, l. 492-521).
- `fft_size = (2·block).next_power_of_two()` ; **bloc codé en dur à 1024** à trois endroits (`outputs/local.rs:1391`, `:3206`, `orchestrator/dsp.rs:1858`). **Latence = 1024 trames** (`latency_frames`, l. 443), soit 21,3 ms à 48 kHz, 10,7 ms à 96 kHz ; `process_offline` la compense.
- **Pas de rééchantillonnage du filtre** : `build_for` refuse une cadence différente (« réexportez le filtre à {sr} Hz », l. 69-74). L'issue #5190 est **fermée « not planned »** le 09/10. En cas d'écart de cadence, le convolueur est retiré et le son passe **sans correction** (`local.rs:1392-1400`).
- Formats : WAV RIFF (PCM 16/24/32, flottant 32/64, `WAVE_FORMAT_EXTENSIBLE`), mono dupliqué ou N canaux = flux ; `combiner_en_stereo` fusionne deux WAV mono ; **aucune limite de taps**, aucun bench de coût.
- `process_block` alloue quatre vecteurs à chaque bloc (l. 492-511) ; `StreamingDsp` enchaîne `process_pcm` → `process_offline` sur une même instance alors que la doc demande une instance fraîche par tampon : **à vérifier** (discontinuités aux frontières de blocs sur le chemin réseau).
- Réglage par zone : clé `ir_path_{zone}`, fichier `$TUNE_DATA_DIR/ir/zone_{id}.wav` (+ `_L`/`_R`) ; routes `/room-correction/ir/upload|clear|status/{zone}` ; visualisation `GET /zones/{id}/convolver/response` (200 points, canal 0).

### 1.2 Égaliseur natif — `sdk/tune-plugin-equalizer`

- Plugin natif cdylib (ABI `tune-plugin-abi` v1, `kind: dsp`), aussi lié statiquement en secours (`tune-core/src/audio/eq.rs:64-90`).
- Mode macro (3 biquads : low shelf 80 Hz, peak 2 kHz Q1, high shelf 10 kHz) **ou** mode expert `bands: Vec<EqBandSpec>` qui remplace tout, **jusqu'à 256 bandes**.
- Types : `peak`, `low_shelf`, `high_shelf`, `low_pass`, `high_pass`, `notch` ; biquads **RBJ Cookbook, forme directe I, f64** ; shelves à pente fixe S = 1 (pas de Q).
- Bornes : `freq` 10 Hz – 0,45·fs ; `q` 0,1 – 30 ; `gain` ±24 dB. Réserve : `headroom_mode: "safe"` (norme L1) ou `"realistic"` (max + 0,25 dB, limiteur).
- **Format d'une bande** : `{ "freq": Hz, "gain": dB, "q": Q, "type": "peak", "channel": 0|1 (optionnel) }`. Corps de `POST /zones/{id}/eq` : `{ enabled, preset?, bands?, headroom_mode? }` (`routes/playback.rs:5125`). Stockage : clé `zone_{id}_eq_profile` (`EqProfile`, 7 champs obligatoires + `bands` + `headroom_mode`).
- C'est **le format de sortie du prototype C2** (`eq::EqSettings`, sérialisé à l'identique, essai `serialisation_au_format_natif`).

### 1.3 Couche « room correction » déjà présente

`tune-core/src/room_correction.rs` : `RoomProfile { name, zone_id, filters: Vec<CorrectionFilter>, measurement_data }`, `CorrectionFilter { frequency_hz, gain_db, q_factor, filter_type }`, clés `room_profile_{zone}` ; un auto-EQ rudimentaire `generate_correction_from_measurements` (cible plate 0 dB absolu, seuil 3 dB, **relances jusqu'à +12 dB**, Q 2 par défaut). Routes `/room-correction/profiles…`, `POST /analyze`, `POST /profiles/{zone}/apply` (garde `Feature::RoomCorrection`, Premium). Les mesures arrivent en JSON de l'extérieur : **rien ne mesure**.

Défaut constaté à la lecture : `apply_profile_handler` (`routes/room_correction.rs:263-276`) et `dac_calibration.rs:206-221` écrivent dans `zone_{id}_eq_profile` un JSON `{enabled, source, profile_name, bands, preamp_db}` **sans** les sept champs obligatoires d'`EqProfile` ; le lecteur `eq_profile_configure_with` (`orchestrator/dsp.rs:1734-1754`) fait `from_str(..).ok()?` et devrait rendre `None`, donc **ne pas appliquer** le profil. À vérifier par exécution avant C2 ; si confirmé, c'est un bug à corriger dans le lot courant, indépendamment de l'étude.

Antériorité hors dépôt : `~/DEV/tune-plugin-room-calibration/room_calibration_plugin.py` (mai 2026, 2 044 lignes, numpy/scipy : balayage log, inverse, bruit rose, MLS, RT60, `.cal` UMIK-1/iMM-6). Abandonné avec les greffons Python ; sert de référence de périmètre, pas de code.

### 1.4 Extension Entrée audio — `plugins/tune-entree-audio`

- Greffon **in-tree** (`TunePlugin`, feature `entree-audio` par défaut), pas l'ABI native. Capture par **cpal** (vendor 0.17.3, feature `capture` ← `local-audio`) : CoreAudio, ALSA, WASAPI.
- Formats F32/I16/I24/I32, convertis en PCM entier 16/24 bits ; **aucune négociation de cadence** : `default_input_config()` du périphérique ; changement de cadence surveillé toutes les 500 ms.
- Horloge : chaque bloc est horodaté par `info.timestamp().capture` (`systeme.rs:220`) ; dérive mesurée en ppm (Yeti X : −6,2 ppm contre l'hôte, `derive.rs:22`) ; anneau de 12 s, amorce 2,5 s.
- Un micro USB (UMIK-1 = périphérique USB Audio Class 1, 48 kHz, 24 bits, mono) **apparaît dans `lister()`**. macOS : autorisation TCC lue dans `autorisation.rs`.
- Livraison : `FournisseurPcm` « en direct » servi comme une radio (`/stream/<id>.wav`) vers une zone. **Aucune API ne rend les échantillons captés à un analyseur** : C1 doit ajouter une consommation « capture vers tampon » (même `FluxDirect`, autre consommateur), pas une nouvelle pile.

### 1.5 Analyse acoustique existante

Aucune : ni RT60, ni balayage, ni déconvolution en production. Ce qui existe : l'analyseur de spectre des VU-mètres (`audio/levels.rs`, FFT radix-2 maison, 2048 points, 32 bandes), la prise PCM par zone `audio/tap.rs` (broadcast du PCM décodé, prévue pour des greffons d'analyse), `fir_frequency_response` (somme directe). `realfft 3.5.0` est déjà une dépendance directe de `tune-core`, `rustfft 6.4.1` transitive : **le prototype n'ajoute aucune dépendance au serveur**.

### 1.6 Profils par zone

Table clé/valeur `settings` : `zone_{id}_eq_profile`, `zone_{id}_crossfeed`, `zone_{id}_audiophile`, `ir_path_{id}`, `room_profile_{id}` ; colonnes `zones.dsp_preset_id` / `dsp_enabled` (migration v25) **sans table `dsp_presets`** référencée. Pas de profil DSP complet nommé regroupant EQ + IR + crossfeed : chaque traitement a sa clé. C4 devra introduire une notion de **profil de correction** (par zone, par position d'écoute) qui pointe à la fois sur des bandes EQ et sur un fichier IR.

### 1.7 Chemin du signal et PURE

- Ordre local (`outputs/local.rs:3305`, `apply_local_dsp`) : **EQ → convolueur → crossfeed → repli mono**, puis volume (chemin natif). Réseau (`transcodage.rs:583-610`, `StreamingDsp::process`) : ReplayGain → EQ → convolueur → crossfeed → compensation. Le convolueur local est bâti à la cadence **décodée** (`dec_sr`), le réseau à `out_sr`.
- **PURE** (`audio/audiophile.rs`, clé `zone_{id}_audiophile`) court-circuite tout : `load_eq_profile` → `None`, `chemin_ir` → `None`, crossfeed `None`, ReplayGain coupé, `set_pure_bypass` sur la sortie locale. La correction de pièce est donc déjà incompatible avec PURE par construction ; il reste à **le dire** : le chemin du signal (`routes/zones/signal_path.rs:700`, `build_signal_path`) n'a **aucune étape « Convolver »/« IR »** (l'étape « DSP » couvre l'EQ ou la sonde runtime) ; un IR seul sur une zone réseau n'y apparaît pas. C1/C2 ajoutent une étape `Correction de pièce` (`bit_perfect: false`, détail : profil, bandes, taps, latence) et un motif `pure_degraded` explicite.

### 1.8 Sorties : où jouer un balayage et le capter en synchronie

| Sortie | Fichier | Horodatage de sortie | Latence | Verdict pour la mesure |
|---|---|---|---|---|
| Locale (cpal) | `outputs/local.rs`, `local/*` | `OutputCallbackInfo` ignoré (l. 2895, 2940), position = trames fournies | tampon cpal par défaut, `TUNE_ALSA_PERIOD_FRAMES` sous Linux, + 1024 trames si convolueur | **Oui** : même machine que le micro USB ; retard fixe mesuré par le balayage lui-même |
| OAAT | `outputs/oaat/*` | **PTS absolu** par paquet (`stream_start_ns = now + 500 ms`), horloge synchronisée | `render_latency_calibrated: false` (`multiroom.rs:213`) | Oui en C4 : le PTS donne t0 ; le retard DAC + acoustique est ce que l'on mesure |
| AirPlay 1 (RAOP) | `airplay.rs`, `airplay_timing.rs` | pas de synchro NTP/RTP (« unsupported ») | ~2 s, inconnue | Mesurable par **repère de synchronisation** seulement ; dérive d'horloge à surveiller |
| AirPlay 2 | `airplay2/` (sous-processus GPL) | aucune | inconnue | idem, basse priorité |
| DLNA / Chromecast / BluOS… | `dlna.rs`, `chromecast.rs` | position SOAP / statut média (s) | de 0,5 à plusieurs s, variable | Repère de synchronisation obligatoire ; jitter de démarrage mais horloge DAC stable ensuite |

Aucun mécanisme de « jouer un signal interne » n'existe (pas de bip, pas de test de haut-parleurs). Le mécanisme générique est **`tune_core::source_pcm::FournisseurPcm`** (utilisé par `tune-cd`, `tune-entree-audio`, essais « faux-cd ») : un fournisseur « balayage » fini, résolu comme ligne de file et servi en `/stream/<id>.wav` vers **n'importe quelle zone**, est la voie naturelle et ne touche pas aux sorties.

### 1.9 Applis mobiles

`tune-remote-flutter` (Android, 1.0.3x) : REST `api/v1` + WebSocket `/ws` ; **aucun** paquet audio, pas de `RECORD_AUDIO`. `tune-remote-ios` (Swift, build 35) : REST + WebSocket ; `AVAudioSession` sert au routage ; **pas de `NSMicrophoneUsageDescription`**, pas d'`AVAudioEngine`. Tout est à créer (§ 2.6).

---

