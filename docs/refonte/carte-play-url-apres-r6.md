# La carte de `play_url` après R6 — matière de R7 et R8

Relevé sur la tête de la PR #3981 (`fix/2219-r6-decoupe-local-b209`,
`210e2a81`, 12 septembre 2026), lue dans un worktree détaché. Rien n'est
estimé : chaque nombre vient d'un `sed -n` ou d'un `grep -c` sur le fichier,
et chaque site est cité `fichier:ligne` sur cette tête. Ce document prépare
deux tranches de l'epic #2219 : **R7** (une boucle producteur commune aux
quatre bras) et **R8** (un trait backend, un bras migré à la fois). Il ne
décide rien ; il relève, et il nomme les décisions qui restent à Bertrand.

## 0. Où vit `play_url` sur cette tête

`LocalOutput::play_url` est à **`tune-core/src/outputs/local.rs:3788–7195`,
3 408 lignes**, dans `impl OutputTarget for LocalOutput` (l. 3702–7471).
Le tableau de bord de #2219 annonce un module `cible_de_sortie` ; sur
`210e2a81` il n'existe pas encore (`ls tune-core/src/outputs/local/` :
`etat_backend.rs`, `parc.rs`, `resolution.rs` et 21 modules de test). Les
chemins ci-dessous sont ceux de la tête relue ; quand la famille
`impl OutputTarget` sera sortie, ils se réadressent d'un préfixe, et les
gardes du §5 avec eux.

`local.rs` sur cette tête : 7 895 lignes, dont 3 408 pour `play_url`
(43 %). Douze attributs `#[cfg` dans la fonction (3835, 3849, 3936, 4179,
4321, 4625, 4874, 5445, 5783, 5838, 5955, 6234).

## 1. Anatomie de `play_url`

### 1.1 Les blocs successifs

| bloc | lignes | taille | ce qui s'y passe |
|---|---|---:|---|
| préambule | 3788–4005 | 218 | `stop()` de la piste précédente (3802), `reset_local_dsp` (3809), attente de relâche du PCM par la sentinelle du fil précédent (3849–3898, hors Windows ; 200/500 ms de sommeil sur Windows 3835–3843), drapeau `force_silent` neuf (3905), génération (3909), effacement de `open_failure` (3929), clonage des 30 `Arc` (3935–3987) |
| fil de lecture : HTTP et en-tête | 4006–4072 | 67 | `std::thread::spawn` (4006), `SentinelleDuFilDeLecture` (4010), GET bloquant (4015), lecture des 4 096 premiers octets (4040–4058), `device_gone` (4071) |
| branche compressée (non-WAV) | 4084–4560 | 477 | lecture de tout le flux (4092–4117), `decode_compressed_stream` (4125), résolution du périphérique (4140), cascade f32 → i32 → i16 (4251–4303), DSP sur la piste entière (4407), `adapt_channels` (4435), `rubato_resample_track` (4444), pré-remplissage (4452), alimentation par tranches (4477–4509), fin naturelle (4518), vidage borné (4527–4552). **Sort par `return` (4559)** |
| format source → type, convolveur | 4562–4622 | 61 | `current_format` (4568), `rebuild_local_convolver` (4572), `bytes_per_sample`/`frame_bytes` calculés à la main (4584–4589), `AudioSpec::depuis_entete` (4603), refus `local_audio_unsupported_source_format` (4604–4622) |
| bras CoreAudio exclusif | 4624–4871 | 248 | §1.2 |
| bras ASIO exclusif | 4873–5442 | 570 | §1.2 |
| bras WASAPI exclusif | 5444–5782 | 339 | §1.2 |
| CPAL partagé : résolution, cadence, ouverture | 5786–6275 | 490 | `find_device_with_fallback` (5791), `decide_local_rate_opening` (5894) et ses quatre bras (5903–6025), `note_rate_decision` (6026), fermeture `build_stream` f32 (6041–6089), anneau (6096), repli cadence source (6116–6149), cascade entière i32/i16 (6167–6214), échec de toutes les tentatives (6217–6257) |
| CPAL partagé : frontière R1, amorce, boucle | 6277–6633 | 357 | `refuser_le_porteur_dop` (6316–6330), rééchantillonneur (6334–6385), `EtageDeConversion` (6399–6421), `PuitsAnneauCpal` (6422–6427), amorce `etage.pousser` (6430), `BoucleProducteur::tourner` (6535–6585), piste vide (6615) |
| gapless | 6635–7015 | 381 | boucle `while http_eof` (6641), GET suivant (6670), en-tête (6703–6736), `AudioSpec` (6744), queue du DSP au format sortant (6775–6785), vidage du rééchantillonneur (6794–6805), `etage.spec = nouvelle_spec` (6815), rééchantillonneur recréé (6846–6891), bascule des métadonnées (6899–6911), amorce enchaînée (6933), `producteur_enchaine.tourner` (6957–6986) |
| fin | 7017–7189 | 173 | `doit_declarer_chaine_epuisee` (7029), queue du DSP (7051–7062), vidage du rééchantillonneur (7066–7081), vidage borné de l'anneau avec position réelle (7111–7147), fin naturelle (7149–7158), `device_gone` → `open_failure` (7163–7175), `drop(stream)` (7177) |
| épilogue | 7190–7195 | 6 | `stop_tx`, `play_thread` |

Cinq chemins sortent du fil par `return` sans repasser par la fin commune :
la branche compressée (4559), CoreAudio (4870), ASIO (5441), WASAPI (5773) et
les refus. Le bloc « fin » (7017–7189) n'est donc la fin que du chemin CPAL
partagé.

### 1.2 Les bras, un par ligne

« Mot » = ce que l'anneau range et ce que le rappel temps réel rend au
pilote. « Décode » = où les octets source deviennent des mots. « Échec » = par
quel `record_*` le fil renseigne `open_failure`, le créneau que
`take_output_failure` (7420) draine.

| bras | lignes | `cfg` et condition | anneau | mot | décode | convertit | écrit | détecte l'arrêt | rapporte l'échec |
|---|---|---|---|---|---|---|---|---|---|
| **CoreAudio exclusif** | 4624–4871 (248) | `#[cfg(target_os = "macos")]` (4625), `if exclusive_mode` (4626) | `RingBuf` f32 (4640), 2 s, `Arc` passé à `ExclusiveOutput::new` (4643, `coreaudio_exclusive.rs:433`) | **f32** jusqu'au rappel `Interleaved<f32>` (`coreaudio_exclusive.rs:506–530`, `ring_for_callback.pop` l. 522, volume multiplié dans le rappel l. 525–527) ; la conversion vers le format physique (16/24/32) est faite par l'AudioUnit, hors du dépôt | `LocalPcmProcessor::process_pcm_chunk` (1938) à 4712 (amorce) et 4764 (boucle) → `pcm_bytes_to_f32` (1976) + `apply_local_dsp` (1977) | aucune : ouvert au format source (`prepare_exclusive_device`, `coreaudio_exclusive.rs:302`) | `feed_ring_abortable` 4714, 4769, 4817 (queue DSP) | `stop_rx` 4728, `force_silent` 4731, EOF 4737 ; vidage borné `drain_deadline_for` 4836–4858 ; **aucun `device_gone`** (pas de rappel d'erreur, `#1626`) | `record_exclusive_open_failure("CoreAudio")` 4654 ; `record_feed_stall_failure("CoreAudio")` 4792 sur `feed_stalled` (4710, 4721, 4776) |
| **ASIO exclusif** | 4873–5442 (570) | `#[cfg(all(target_os = "windows", feature = "asio"))]` (4874), `exclusive_mode && audio_backend == "asio"` (4875) | **deux** : `RingBuf` f32 (4889) et `NativePcmRing` i32 (4891) ; l'un des deux retenu par `uses_native_transport()` (4918) → `WindowsExclusiveRingRef` (2571) | **selon le pilote** (`AsioTransport`, `asio_exclusive.rs:175`) : `NativeI32`/`NativeI24`/`NativeI16` = i32 aligné à gauche rendu tel quel ou décalé (`native_ring.pop` l. 509 ; `pop_mapped` avec `>> 8` l. 534, `>> 16` l. 558) ; `ProcessedI32/I24/I16/F32` = f32 converti **dans le rappel** par `pop_mapped`, volume en f64 (l. 581–582, 611–612, 641–642 ; `float_ring.pop` l. 670) | route native : `prepare_windows_native_pcm` (3178) — `pcm_bytes_to_native_i32` (3201) si identité ou DoP, sinon `pcm_bytes_to_f32` + DSP + volume + `f32_to_native_i32` (3203–3220) ; route flottante : `prepare_windows_exclusive_pcm` (2467). Appelées par `feed_selected_windows_exclusive_leftover` 4984 et 5225 ; plus `pcm_bytes_to_native_i32` direct à 5306 (reliquat 24 bits court) | aucune (format source) | 4984, 5225 (via `feed_*`), `feed_native_ring_abortable` 5307, `feed_selected_windows_exclusive_tail` 5353 | lecture HTTP sur un **fil pompe** séparé (5084–5120, `sync_channel(64)`) ; `stop_rx` 5125, `force_silent` 5128, EOF 5155, **EOF par inactivité 5 s** (5176–5199) ; vidage doublement borné 5382–5426 (`asio_drain_timeout`) ; pas de `device_gone` | `record_exclusive_open_failure("ASIO")` 4907 ; `record_windows_exclusive_pcm_refusal("ASIO")` 5038, 5327 ; **aucun `record_feed_stall_failure`** : le verdict de `feed_native_ring_abortable` est jeté (2636) et celui de `feed_ring_abortable` aussi (2557) |
| **WASAPI exclusif** | 5444–5782 (339) | `#[cfg(target_os = "windows")]` (5445), `exclusive_mode && audio_backend != "asio"` (5446) | `NativePcmRing` i32 (5459), `Arc` passé à `WasapiExclusiveOutput::new` (5462, `wasapi_exclusive.rs:344`) | **i32 aligné à gauche**, sérialisé en octets par `pop_pcm_bytes` sur le fil de rendu (`wasapi_exclusive.rs:677`, `789` → `native_i32_to_pcm_bytes` 2984) ; **volume appliqué par le producteur** (3214–3219, 5723–5729), jamais dans le rappel | `feed_windows_native_exclusive_leftover` 5524 (amorce), 5608 (boucle) → `prepare_windows_native_pcm` (3178) ; `pcm_bytes_to_native_i32` 5695 (reliquat 24 bits) ; `f32_to_native_i32` 5730 (queue DSP) | aucune (format source) | 5524, 5608 (via `feed_*`), `feed_native_ring_abortable` 5696, 5731 | `stop_rx` 5592, `force_silent` 5595, EOF 5601 ; **vidage non borné** 5751–5762 (seul bras sans délai) ; pas de `device_gone` | `record_exclusive_open_failure("WASAPI")` 5473 (échec de `start`), 5777 (échec de `new`) ; **aucun `record_feed_stall_failure`** (verdict jeté à 2636) |
| **CPAL partagé** (WAV) | 5786–7189 (1 404, dont gapless 381 et fin 173) | aucun `cfg` de bras ; 5838 et 6234 (`linux`), 5955 (`macos`) à l'intérieur | `RingBuf` f32 (6096 ; 6127 et 6174 selon la cascade) | **f32** (`build_stream` 6041–6089, `ring_cb.pop` 6079, `SoftMuteRamp` 6060–6081) **ou i32/i16** par `build_int_stream` (6178, 6191) : `into_sample` dans `render_local_shared_integer_callback` (2173), volume dans le rappel (2172) | `EtageDeConversion::decoder` (3358) → `process_pcm_chunk` (1928) ; un seul site, appelé par `pousser` (3392) | `convertir` (3364) : `adapt_channels` (3366) **puis** `rubato_resample_chunk` (3369) | `puits.ecrire` dans `pousser` (3401) et `rendre_la_queue_du_dsp` (3421) ; deux vidages hors route : 6803, 7079 | `BoucleProducteur::tourner` : `stop_rx` 3512, `force_silent` 3522, `device_gone` 3535, EOF 3557 ; vidage borné 7111–7147 avec position réelle (7141–7145) | `record_shared_device_not_found` 5797 ; `open_failure` direct 6249 (cascade épuisée) et 7164 (`device_gone`) ; `refuser_le_porteur_dop` 6323 ; `record_feed_stall_failure("CPAL")` 3676 sur `PuitsMort` |
| compressé (non-WAV, hors REF-8 : f32 décodé d'un bloc) | 4084–4560 (477) | aucun ; 4179, 4321 (`linux`) | `RingBuf` f32 (4258) | f32/i32/i16 par `cascade_de_formats` (4251) : `build_compressed_f32_stream` 4262, `build_int_stream` 4274, 4286 | `decode_compressed_stream` (4125, symphonia, piste entière) | `apply_local_dsp` 4407, `flush_local_dsp` 4422, `adapt_channels` 4435, `rubato_resample_track` 4444 | `ring.push` 4452 (pré-remplissage), `feed_ring_abortable` 4486 | 4478 ; vidage borné 4527–4552 | `record_compressed_decode_failure` 4128, `record_shared_device_not_found` 4146, `open_failure` 4337 ; famine : `warn!` seul à 4496, **pas de `record_feed_stall_failure`** |

Trois faits que la lecture ligne à ligne ajoute aux corps de PR :

- **Le DoP suit trois régimes.** CPAL partagé le **refuse** (6316, #3233) ;
  ASIO route flottante le **refuse** (`DopUnsupported`, 2483) mais ASIO route
  native et WASAPI l'**acceptent** et le verrouillent (`dop_latched`, 2635) ;
  CoreAudio ne le refuse ni ne le verrouille : `process_pcm_chunk` le
  reconnaît (`bloc_est_porteur_dop`, 1866, appelé à 1962), fige le volume à
  l'unité (`sync_volume_to_dop`, 1972) et le laisse traverser **en f32** jusqu'à
  l'AudioUnit. Un `f32` porte un mot de 24 bits sans perte (mantisse de
  24 bits) ; l'exactitude de la reconversion f32 → 24 bits par l'AudioUnit
  n'est mesurée nulle part dans le dépôt.
- **Le volume s'applique à trois endroits différents.** Dans le rappel pour
  CPAL partagé (6080), CoreAudio (`coreaudio_exclusive.rs:525`) et ASIO
  `Processed*` (l. 581, 611, 641) ; dans le **producteur** pour WASAPI et ASIO
  `Native*` (`prepare_windows_native_pcm`, 3214). Un puits natif unique
  impose de choisir.
- **Deux bras sur cinq rapportent la famine.** `record_feed_stall_failure`
  n'est appelé que par CPAL (3676) et CoreAudio (4792). ASIO et WASAPI
  jettent le verdict `false` de leurs `feed_*` (2557, 2636) ; le compressé
  le lit (4493) mais ne renseigne pas `open_failure`. ASIO se rattrape par
  `asio_drain_timeout` (5407, 5416) ; WASAPI n'a **aucune** borne de
  vidage (5751–5762).

## 2. Ce que R1 et R5 ont unifié, ce qui reste dupliqué

R1 (#3958) a réduit le chemin CPAL partagé à une route :
`EtageDeConversion::pousser` (3386) = décoder → refuser DoP → convertir →
écrire, et `BoucleProducteur::tourner` (3496) pour les deux pistes. R5
(#3965) a typé cette route (`AudioSpec`, `BlocPcm`, `FormatOuvert`) et a
remonté `AudioSpec::depuis_entete` **au-dessus** des bras (4603), si bien que
les trois bras exclusifs reçoivent un `spec` (4712, 4764) sans s'en servir
autrement que pour `process_pcm_chunk`. `frame_bytes` reste une variable
locale (4589), lue par les trois bras exclusifs (4758, 4954, 4986, 5227,
5305, 5526, 5610, 5693).

Sites **restants** par geste, hors de la route R1 (un site = un appel dans
`play_url` ou dans une aide qu'un seul bras appelle) :

| geste | CPAL partagé | compressé | CoreAudio | ASIO | WASAPI | hors route R1 |
|---|---|---|---|---|---|---:|
| lire l'amont (`.read(&mut`) | 1 (`tourner` 3556) | 1 (4102) | 1 (4736) | 1 (pompe 5088) | 1 (5600) | **4** |
| détecter l'arrêt (`stop_rx` + `force_silent`) | 1 (3512–3531) | 1 (4478) | 1 (4728–4734) | 1 (5125–5131) | 1 (5592–5598) | **4** |
| détecter le périphérique perdu (`device_gone`) | 1 (3535) | 1 (4493, 4540) | 0 | 0 | 0 | 0 — les exclusifs n'ont pas de rappel d'erreur |
| sauter les octets d'un seek | 1 (3622–3636) | 0 (décodeur pré-positionné) | 0 (`seek_offset` ne sert qu'à la position, 4783) | 1 (4952–4958, 5213–5223) | 0 (idem, 5671) | **1** |
| décoder | 1 (`decoder` 3358) | 1 (4125) | 2 (4712, 4764) | 3 (4984, 5225, 5306) | 3 (5524, 5608, 5695) | **9** |
| refuser ou accepter le DoP | 1 (6316 → 3396) | 0 (sans objet) | 0 (traverse) | 1 refus (2482) + 1 sonde de fin (5294) + 2 rapports (5038, 5327) | 0 (accepté, 3196) | **4** |
| adapter les canaux | 1 (3366) | 1 (4435) | 0 | 0 | 0 | **1** |
| rééchantillonner | 1 (3369) + 2 vidages (6795, 7071) | 1 (4444) | 0 | 0 | 0 | **3** |
| convertir en mot natif | rappel (2173, 2214) | même rappel | 0 (AudioUnit) | producteur 3201/3220 + rappel `pop_mapped` ×5 (`asio_exclusive.rs:534, 558, 582, 612, 642`) + `f32_to_native_i32` 2735 | producteur 3201/3220, 5695, 5730 ; rappel 677/789 | **9** |
| appliquer le volume | rappel (6080, 2172) | rappel | rappel (`coreaudio_exclusive.rs:525`) | rappel (`Processed*`, l. 581, 611, 641) **ou** producteur (`Native*`, 3214) | producteur (3214, 5723) | 3 régimes |
| écrire dans l'anneau | 2 (3401, 3421) + 2 vidages (6803, 7079) | 2 (4452, 4486) | 3 (4714, 4769, 4817) | 4 (4984, 5225, 5307, 5353) | 4 (5524, 5608, 5696, 5731) | **15** |
| rendre la queue du DSP (`flush_local_dsp`) | 2 (6776, 7051) | 1 (4422) | 1 (4808) | 1 (5344) | 1 (5714) | **4** (6 appels + 1 définition : compte de `dsp_track_boundary:67`) |
| drainer l'anneau en fin de piste | 1 (7114–7147, borné) | 1 (4530–4552, borné) | 1 (4839–4858, borné) | 1 (5393–5426, deux bornes) | 1 (5751–5762, **sans borne**) | **4** |
| détecter la famine et la dire | 1 (3657–3682) | 0 (`warn!` 4496) | 1 (4787–4798) | 0 (verdict jeté) | 0 (verdict jeté) | 1, et 3 bras muets |
| rapporter l'échec d'ouverture | 2 (5797, 6249) | 2 (4146, 4337) | 1 (4654) | 1 (4907) | 2 (5473, 5777) | **6** |
| signaler la fin naturelle | 1 (7149–7153) | 1 (4518–4520) | 1 (4824–4828) | 1 (5367–5371) | 1 (5744–5748) | **4** |
| calculer la position | 1 (3693) | 1 (4504) | 1 (4782) | 1 (5286) | 1 (5668) | **4** |
| basculer le volume DoP (`sync_volume_to_dop`) | 1 (1972) + 1 (6328) | 0 | via 1972 | 6 (4971, 5006, 5046, 5056, 5247, 5335) | 4 (5516, 5548, 5582, 5635) | **10** |
| publier le contrat de signal (`publish_windows_signal_path_status`) | 0 (REF-6b côté producteur, après R6) | 0 | 0 | 2 (5014, 5255) | 2 (5556, 5643) | **4** |
| journaliser (`info!`/`warn!`/`debug!`) | 12 + 12 + 13 + 5 (ouverture, boucle, gapless, fin) + 11 (`tourner`) | 17 | 6 | 17 | 11 | 105 dans `play_url` |

Lecture : les gestes **décoder, écrire, drainer, dire la fin, calculer la
position** existent en cinq exemplaires (un par chemin) ; R1 n'en a réduit
qu'un. C'est la matière de R7. Les gestes **convertir en mot natif** et
**appliquer le volume** n'ont pas d'exemplaire unique parce que chaque bras
l'a placé d'un côté différent de l'anneau ; c'est la matière de R8, et elle
demande une décision avant une ligne de code.

## 3. La table de migration R7c / R8 : chaque bras vers `PuitsNatif`

`PuitsNatif` (#3985, `tune-output-api/src/lib.rs:1444`) reçoit un
`BlocPcm<'_>` — des **octets** et leur `AudioSpec` — et rend `false` quand le
puits est mort. `CaptureOutputNatif` (l. 1529) refuse un bloc dont la spec
diffère de celle d'ouverture (`RefusNatif::SpecDifferente`, l. 1464), garde
le reste non aligné (l. 1612) et hache le flux. Il ne renifle pas le DoP et
ne convertit rien.

Conséquence immédiate pour les deux bras Windows : leur anneau range des
**i32** (`NativePcmRing`, 1097), pas des octets. Aujourd'hui la route
bit-perfect fait octets → i32 (`pcm_bytes_to_native_i32`, 3201) → anneau →
i32 → octets (`native_i32_to_pcm_bytes`, 2984, dans `pop_pcm_bytes`). Un
puits qui prend un `BlocPcm` supprime ces deux conversions sur la route
identité ; il en ajoute une sur la route traitée (f32 → octets), qui n'existe
pas encore dans le dépôt (`f32_to_native_i32` + `native_i32_to_pcm_bytes`,
ou une fonction neuve).

| | CoreAudio | WASAPI | ASIO |
|---|---|---|---|
| **sites à toucher dans `play_url`** | 4640 (anneau), 4643–4651 (`ExclusiveOutput::new` prend l'`Arc<RingBuf>`), 4712/4764 (décodage : identité → bloc brut ; DSP ou volume ≠ 1 → f32 → mot natif), 4714/4769/4817 (écritures), 4808–4819 (queue DSP → mot natif), 4836–4858 (vidage lit `ring.available()`) | 5459 (anneau), 5462–5470 (`new` prend l'`Arc<NativePcmRing>`), 5524/5608 (`feed_windows_native_exclusive_leftover` rend des `Vec<i32>`), 5692–5710 (reliquat 24 bits), 5714–5738 (queue DSP), 5751–5762 (vidage) | 4889–4893 (deux anneaux), 4895–4904 (`new`), 4918–4922 (`WindowsExclusiveRingRef`), 4984/5225 (`feed_selected_*`), 5291–5325 (fin de sonde), 5344–5362 (queue), 5382–5426 (vidage lit `selected_ring`) ; et le fil pompe 5079–5121, qui est déjà la moitié d'un producteur séparé |
| **sites à toucher dans le module** | `coreaudio_exclusive.rs:433–441` (signature), `506–530` (rappel `Interleaved<f32>` → format entier natif si le puits est natif, ou conversion octets → f32 dans le puits si le rappel reste flottant) | `wasapi_exclusive.rs:344–351` (signature), `674–677` et `781–789` (`pop_pcm_bytes` devient une copie d'octets) | `asio_exclusive.rs:242–248` (signature), `487–700` (`build_native_stream` : les quatre variantes `Processed*` convertissent et appliquent le volume dans le rappel, l. 581–642 ; les trois `Native*` décalent le mot, l. 509–558) |
| **spec du `BlocPcm`** | `AudioSpec::depuis_entete(sample_rate, bit_depth, channels)` (4603) = format source = format physique demandé (`RequestedPhysicalFormat`, `coreaudio_exclusive.rs:83`) ; `bit_depth == 0` (flottant IEEE, 4583) → `ProfondeurPcm::FlottantIeee32` | même `spec` (4603) ; le format ouvert est vérifié par `wasapi_exclusive.rs:344+` contre le mix format exclusif | même `spec` ; le **mot rendu au pilote dépend du transport** (`AsioTransport`, l. 175) : `NativeI24` sort du jeu fermé `{f32, i16, i24, i32}` par le côté `Entier24`, `ProcessedI16` par `Entier16` ; la spec du bloc n'est plus celle de la source |
| **où la décision DoP se prend** | `process_pcm_chunk` (1938) : `LocalPcmKind` posé après 32 trames (1962–1966), `sync_volume_to_dop` (1972). Aucun refus. À migrer, la décision reste **dans le producteur** ; le puits reçoit des octets 24 bits identiques | `prepare_windows_native_pcm` (3196) : `is_dop_pcm` sur 24 bits, verrouillé par `dop_latched` (2635). Reste dans le producteur | route native comme WASAPI ; route flottante : refus `DopUnsupported` (2482) rapporté à 5038/5327. Reste dans le producteur ; le refus doit survivre à la migration |
| **témoins existants qui gardent ce bras** | `tune-server/tests/refus_exclusif_dit_sa_cause_i3108.rs` : 4 tests sur le bloc CoreAudio (`bloc_coreaudio_exclusif` l. 35 ; `feed_stalled = true` ×2 l. 86 ; `if feed_stalled {` l. 94 ; `position_ms.load(` l. 104 ; `let ring_cap = (sample_rate as usize) * (channels as usize) * 2;` l. 119 ; `drain_deadline_for(` l. 134) et `les_trois_transports_exclusifs_arment_le_canal_sur_un_refus_d_ouverture` (l. 67) · `tune-core/tests/dsp_track_boundary.rs:84–93` (le bloc CoreAudio contient `pcm_processor.process_pcm_chunk(` et `flush_local_dsp(`) · `tune-core/src/outputs/local/backend_fallback_tests.rs:391` (`note_opened_device(` avec `"CoreAudio"`, fenêtre sur le marqueur `_playing`) · `coreaudio_exclusive.rs:757+` (HAL factice : hog, formats, rollback) · `tune-core/src/outputs/local/tests.rs:2527` (`exclusive_open_failure_is_returned_without_authorising_a_fallback`) | `tune-core/src/outputs/local/tests.rs` : `native_windows_ring_preserves_every_dop_marker_and_payload_byte` (l. 2590), `native_windows_ring_is_byte_exact_for_16_24_and_32_bit_pcm` (l. 2540), `native_windows_preparation_keeps_identity_pcm_out_of_float` (l. 2611), `…forces_dop_onto_the_raw_branch` (l. 2642), `…marks_processed_pcm_as_not_bitperfect` (l. 2676), `versioned_dop_fixture_is_the_real_encoder_output_byte_for_byte` (l. 599, fixture `tests/fixtures/dop_stereo_24le_64frames.hex`) · `dsp_track_boundary.rs:131–140` (`feed_windows_native_exclusive_leftover(` + `flush_local_dsp(`) · `refus_exclusif_i3108.rs:67` (`"WASAPI"`) · `ringbuf_tests.rs:291, 300` (durée alignée, HRESULT) · `tests.rs:2476–2515` (endpoints) · `backend_fallback_tests.rs:391` (`"WASAPI"`) | `tests.rs:2557` (`native_windows_ring_is_exact_at_asio_i16_and_i24_callback_boundaries`), `2700` (`windows_float_exclusive_rejects_dop_before_the_ring`), `2746`, `2756`, `2773` (route flottante) · `asio_exclusive.rs:807+` (`i24_borne` ×3, `bit_perfect_unavailable_reason`) · `dsp_track_boundary.rs:117–126` (`feed_selected_windows_exclusive_leftover(` + `flush_local_dsp(`) · `refus_exclusif_i3108.rs:67` (`"ASIO"`) · `zone_backend_asio_i1770.rs`, `enumeration_asio_occupee_tests.rs` · `backend_fallback_tests.rs:391` (`"ASIO"`) |
| **ce qui n'a aucun témoin** | l'exactitude f32 → 24 bits de l'AudioUnit ; la famine (`record_feed_stall_failure("CoreAudio")` n'est vérifié que par texte) | le vidage sans borne (5751) ; l'absence de `record_feed_stall_failure` | idem ; la sortie EOF par inactivité (5176–5199) ; le fil pompe |
| **quelle machine compile** | **le Mac** : job `macos-pr` (`ci.yml:425`), commande l. 440 `cargo check --package tune-server --features dj,karaoke,bandcamp,plugins-wasm` ; Shrek ne compile pas ce bras (#3981 §5 : sabotage vert sur Shrek, rouge sur le Mac) ; **Bertrand peut l'écouter** | **la CI Windows seule** : job `windows-pr` (`ci.yml:384`), étape « Windows livré sans ASIO » l. 403–404 ; ni Shrek ni le Mac. Les aides `cfg(any(windows, test))` (`NativePcmRing`, `prepare_windows_native_pcm`, `pcm_bytes_to_native_i32`, 1096–3246) tournent sur Shrek sous `cargo test` ; les `feed_*` (2517–2739) et le bras lui-même, non | **l'étape « ASIO » seule** (`ci.yml:412–413`, après téléchargement du SDK l. 405) : le bras qu'aucune machine locale ne compile — c'est lui qui a rougi #3981 (E0433 ×4, `super::asio_exclusive`) |
| **compile sous `cfg(test)` sur Shrek ?** | non | aides oui, bras non | aides oui, bras non |

### Ordre proposé

1. **CoreAudio d'abord.** Le Mac le compile (`macos-pr`), Bertrand peut
   l'écouter, il n'a qu'un anneau et un mot (f32), aucune route flottante à
   préserver, et sa migration force la première décision (§4 : le mot que le
   rappel rend). Sa boucle (4727–4785) est la plus proche de
   `BoucleProducteur::tourner` : mêmes témoins d'arrêt, même `read`, même
   `process_pcm_chunk` ; seul le puits diffère.
2. **WASAPI ensuite.** Il est déjà sur un anneau natif et un mot entier ; la
   migration retire deux conversions sur la route identité et n'en ajoute
   qu'une sur la route traitée. Personne ne l'écoute avant les testeurs
   (JP Borderies, DEvir) ; la CI Windows est la seule porte.
3. **ASIO en dernier.** Deux anneaux, sept variantes de transport, un fil
   pompe, et le refus DoP de la route flottante à conserver. C'est le bras
   que ni Shrek ni le Mac ne compilent.

R7c (une boucle commune) précède R8 sur chaque bras : la boucle de
`tourner` (3496) n'a besoin que d'un `Read`, d'un étage et d'un puits. Ce
qui la rend spécifique au chemin partagé est **l'étage** (`EtageDeConversion`
rend des f32 et refuse le DoP), pas la boucle. Un étage natif — décoder par
`prepare_windows_native_pcm`, écrire un `BlocPcm` — suffit pour que les trois
boucles exclusives (4727–4785, 5124–5289, 5591–5686) disparaissent.

## 4. Le trait backend minimal de REF-8, esquissé

À partir de ce que les quatre bras font réellement — ouvrir, configurer,
rendre, observer — et de rien d'autre. Ce bloc est une proposition, pas une
signature à copier.

```rust
/// Ce qu'un backend a réellement ouvert. `spec` est le format des octets
/// que le puits attend ; pour le chemin partagé, c'est le format de
/// l'anneau flottant (`FormatOuvert`) et non celui de la source.
pub struct SessionOuverte {
    pub spec: AudioSpec,
    pub peripherique_ouvert: String,        // note_opened_device (4671, 4937, 5492)
    pub endpoint_id: Option<String>,        // Some pour WASAPI seul (5496)
    pub bit_perfect_indisponible: Option<&'static str>, // ASIO (4923)
}

/// Pourquoi rien ne s'est ouvert. Porte le texte que `record_exclusive_open_failure`
/// (2060) et `record_shared_device_not_found` (2376) écrivent aujourd'hui.
pub struct RefusDOuverture { pub backend: &'static str, pub cause: String }

/// Ce que le fil de lecture relève pendant la piste, sans verrou.
pub struct Observation {
    pub mots_en_attente: usize,             // ring.available() (4846, 5400, 5758, 7121)
    pub capacite: usize,                    // selected_ring.capacity() (5383)
    pub peripherique_perdu: bool,           // device_gone (7126) — faux pour les exclusifs
    pub sous_alimentations_pilote: u64,     // record_driver_underrun (7514), underrun_count
    pub erreurs_de_rappel: u64,             // callback_error_count (asio, wasapi)
}

pub trait BackendDeSortie {
    /// Réserve le périphérique et pose le rappel temps réel. Ne démarre pas.
    fn ouvrir(&mut self, demande: AudioSpec, endpoint: Option<&str>)
        -> Result<SessionOuverte, RefusDOuverture>;

    /// Le puits que le producteur alimente. Le backend le POSSÈDE ; le
    /// rappel en tient l'autre bout. Pas de `Box<dyn>` ni de `Mutex` sur
    /// le chemin du rappel : le puits est l'écrivain d'un anneau SPSC.
    fn puits(&mut self) -> &mut dyn PuitsNatif;

    /// Démarre le rendu quand le pré-remplissage est atteint (6510, 6568).
    fn demarrer(&mut self) -> Result<(), String>;

    fn observer(&self) -> Observation;

    /// Attend que l'anneau soit vide, borné. Rend ce qui reste.
    fn drainer(&mut self, delai: std::time::Duration, arret: &dyn Fn() -> bool) -> usize;

    /// Libère le périphérique. `Drop` fait la même chose (4861, 5432, 5764).
    fn fermer(&mut self);
}
```

Ce que chaque bras y mettrait :

| méthode | CoreAudio | ASIO | WASAPI | CPAL partagé |
|---|---|---|---|---|
| `ouvrir` | `prepare_exclusive_device` (hog, format physique, rollback) + `set_render_callback` (`coreaudio_exclusive.rs:302, 506`) | `try_with_asio_device_lock` (l. 59), choix du transport (l. 175), `build_native_stream` (l. 487) | `resolve_wasapi_endpoint` (l. 23), `Initialize` en mode exclusif événementiel (l. 500–504, vtable l. 279), `start` (l. 650) | `find_device_with_fallback` (5791), `decide_local_rate_opening` (5894), cascade f32 → i32 → i16 × deux cadences (6102–6214) |
| `puits` | un anneau natif au format physique (aujourd'hui `RingBuf` f32, 4640) | `NativePcmRing` (4891) ; la route flottante devient un étage producteur, pas un second puits | `NativePcmRing` (5459), déjà en place | `PuitsAnneauCpal` (3247) reste un `PuitsDEchantillons` : c'est le chemin DSP, le mot y est f32 par construction |
| `demarrer` | `audio_unit.start` (aujourd'hui dans `new`, l. 542) | `stream.play` (dans `new`) | `start` (5472) | `stream.play` (6510, 6568, 6623) |
| `observer` | `ring.available()` seulement — aucun compteur, aucun rappel d'erreur | `available`, `capacity`, `underrun_count` (l. 468), `callback_error_count` (l. 472) | `underrun_count`, `deadline_miss_count`, `callback_error_count` (l. 884–893) | `available`, `device_gone` (7494–7505), `record_driver_underrun` (7514) |
| `drainer` | 4836–4858 | 5382–5426 | 5751–5762 (à borner) | 7111–7147 |

### Les décisions qui reviennent à Bertrand

1. **Le mot du puits par bras.** Le fil C a pris l'option A pour lui seul :
   un second trait sur `BlocPcm`, le puits flottant restant le chemin DSP.
   Reste à trancher pour chaque bras exclusif : CoreAudio rend aujourd'hui
   du **f32** à l'AudioUnit et laisse macOS convertir ; le passer au puits
   natif change le mot rendu (rappel `Interleaved<i32>` ou `<i16>` au lieu
   de `<f32>`) — c'est une modification audible en principe, et c'est
   pourquoi il faut l'écouter. ASIO `Processed*` convertit **dans le
   rappel** en f64 (l. 582, 612, 642) ; un puits natif déplace cette conversion dans
   le producteur, en f32, comme WASAPI le fait déjà (3220). Deux régimes de
   précision, un seul doit rester.
2. **Qui possède l'anneau.** Aujourd'hui `play_url` le crée (4640, 4889,
   4891, 5459, 6096) et en passe un `Arc` au backend ; les boucles de
   vidage lisent `ring.available()` depuis `play_url`. Le trait ci-dessus
   le donne au backend et ne rend qu'un puits et une observation. Cela
   supprime `WindowsExclusiveRingRef` (2571) et les cinq boucles de vidage,
   mais déplace `ring_cap = sample_rate × channels × 2` (4638, 4887, 5457,
   6093) — gardé par texte dans `refus_exclusif_i3108.rs:119`.
3. **Le rappel temps réel sans allocation ni verrou (#2206).** Les rappels
   actuels lisent des atomiques (`paused`, `volume`, `force_silent`,
   `data_started`) et un anneau SPSC sans verrou (`ringbuf_tests::drains_temps_reel_ne_font_aucune_allocation`,
   l. 116 ; `le_rappel_entier_local_partage_rend_une_periode_sans_allouer`,
   l. 167). `feed_ring_abortable` (7537) dort côté producteur, jamais côté
   rappel. Un trait objet (`&mut dyn PuitsNatif`) côté producteur ne coûte
   rien au rappel ; mais si le volume et la rampe anti-« ploc » (#1590,
   6060–6081) restent dans le rappel, ils doivent y rester **par valeur**,
   pas derrière le trait. Décider **où s'applique le volume** (§1.2, second
   fait) tranche en même temps ce point : dans le producteur, le rappel ne
   fait plus qu'un `pop` ; dans le rappel, chaque backend garde sa propre
   multiplication et le puits natif n'est « natif » que pour la route
   identité.

## 5. Les gardes de texte qui lisent `local.rs`

`git grep -n 'include_str!\|read_to_string' -- tune-core tune-server | grep -i local`
sur `210e2a81` : 38 lignes. Cinq de `parc.rs` lisent `/proc` et `/etc` (pas
des gardes), deux sont des fixtures `.hex` (`tests.rs:592, 699`), cinq sont
des commentaires, trois ont retenu « local » dans un autre nom
(`orchestrator/annonce_apres_sortie_guard.rs:31`,
`orchestrator/recreation_locale_guard.rs:13`,
`routes/zones/backend_local_annonce_tests.rs:89`). Restent **23 lecteurs de
code : 17 lisent `local.rs`**, 6 lisent les trois modules sortis par R6
(`parc.rs`, `resolution.rs`, `etat_backend.rs`) :

| garde | lit | tests | ce qu'elle compte dans `play_url` ou ses aides | ce que R7c devra étendre |
|---|---|---:|---|---|
| `tune-server/tests/refus_exclusif_dit_sa_cause_i3108.rs:29` | `local.rs` entier | 6 | bloc CoreAudio (`bloc_coreaudio_exclusif` l. 35, entre les deux bannières `// ------- Exclusive mode path`) : `feed_stalled = true` ×2, `if feed_stalled {`, `position_ms.load(`, `let ring_cap = … * 2;`, `drain_deadline_for(` ; les trois `record_exclusive_open_failure(` avec `"CoreAudio"`/`"ASIO"`/`"WASAPI"` et `&open_failure` (l. 67) ; `    fn tourner(` … `\n#[async_trait::async_trait]` contient `record_feed_stall_failure(` et les deux noms d'événement (l. 156–190) ; `fn take_output_failure(&self) -> Option<String> {` + `self.open_failure.lock()` ; **absence** `!LOCAL.contains(".emit(")` (l. 208) | quand la boucle CoreAudio disparaît, les quatre assertions sur `feed_stalled` et le vidage deviennent des assertions sur la boucle commune, avec le nom de backend porté par `record_feed_stall_failure` ; l'assertion d'absence exige de lire **tous** les fichiers où `play_url` et ses aides vivront (concaténation, jamais remplacement) |
| `tune-server/tests/famine_pilote_3205.rs:39` | `local.rs` | 1 | corps de `make_stream_error_cb` (7494) : `audio_stream_error`, `record_driver_underrun` armé **avant** le `warn!` (l. 108–122) | suit `make_stream_error_cb` ; si `observer` absorbe le compteur, l'assertion porte sur l'impl CPAL |
| `tune-server/tests/garde_de_site_porteur_dop_3233.rs:52` | `local.rs` coupé à `#[cfg(test)]\nmod tests` | 4 | fermeture `refuser_le_porteur_dop = |` (6316) et son corps ; `fnconvertir(&mutself,mutmots:Vec<f32>)->Vec<f32>{` avec `adapt_channels(&mots,self.spec.canaux(),self.sortie.canaux)` puis `rubato_resample_chunk(` ; `etage.pousser(` ≥ 2 ; ordre dans `fnpousser(` : `ifrefuser_le_porteur_dop(` < `self.convertir(` < `puits.ecrire(` ; `decide_local_rate_opening(`, `enumerated.is_some(),`, `note_rate_decision(ObservedRate{` | la coupe au premier `#[cfg(test)]\nmod tests` (l. 58) suit le fichier qui portera `play_url` ; un étage natif ajoute une seconde route « refuser puis convertir » que cette garde devra nommer, ou dont elle devra prouver l'absence sur la route native (le DoP n'y est pas refusé) |
| `tune-server/tests/journal_pcm_alsa_ouvert.rs:24` | `local.rs` | 3 | sites d'ouverture = chaque `find_device_with_fallback(` jusqu'au `.build_output_stream(` suivant, `endpoint_id = %` dedans ; `local_audio_compressed_open_endpoint` (4174) ; bras `LocalRateOpening::` (5906–6024) | la fenêtre du site 0 court de 4140 à 6054 (constat de la relecture R6) ; si `ouvrir` devient une méthode par backend, les fenêtres se ferment d'elles-mêmes et la garde doit les rouvrir sur l'impl CPAL |
| `tune-server/tests/refus_de_peripherique_partage_dit_pourquoi.rs:40` | `local.rs` coupé au module de test | 4 | `record_shared_device_not_found(` avant chaque `playing.store(false` des deux chemins partagés (5797, 4146) ; corps de `record_shared_device_not_found` ; `fn take_output_failure(` contient `open_failure` | suit `record_shared_device_not_found` et `take_output_failure` |
| `tune-server/tests/echec_de_decodage_dit_pourquoi_i3270.rs:30` | `local.rs` | 5 | bloc `decode_compressed_stream(&all_data)` → `return;` (4125–4131) : `record_compressed_decode_failure`, `open_failure`, `playing.store(false` ; **`\nfn record_` = 5** (l. 120) | le compte `\nfn record_` casse au premier `pub(super) fn record_` ou au premier `record_*` déplacé — noté par #3981 §7 ; si un `record_feed_stall_failure` commun apparaît pour les cinq chemins, le compte reste 5 seulement s'il remplace et ne s'ajoute pas |
| `tune-server/tests/teneur_du_pcm_branche_3575.rs:25` | `local.rs` coupé au module de test | 3 | `journaliser_les_teneurs_du_pcm(&pcm_ouvert` aux deux chemins d'échec (4323, 6236) ; `#[cfg(target_os="linux")]fnjournaliser_les_teneurs_du_pcm(` | suit les deux échecs d'ouverture partagés ; un `ouvrir` commun les réunit en un site, le compte de 2 tombe à 1 |
| `tune-core/tests/dsp_track_boundary.rs:16` | `local.rs` coupé à `mod tests` | 6 | `reset_local_dsp(&self.convolver)` dans `play_url` ; **`flush_local_dsp(` = 6 + 1** ; `apply_local_dsp(` dans `impl LocalPcmProcessor`, `prepare_windows_exclusive_pcm`, `prepare_windows_native_pcm` ; les **quatre bannières** `// ------- Exclusive mode path (macOS only) -------`, `… (Windows ASIO) …`, `// ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------`, `// ------- Open cpal device (shared mode) -------` découpent le fichier et chaque bras doit contenir son `feed_*`/`process_pcm_chunk` **et** `flush_local_dsp(` ; `etage.pousser(`, `producteur_enchaine.tourner(` + `&mut etage,`, `pousser` contient `self.decoder()` et `puits.ecrire(`, `decoder` contient `process_pcm_chunk(` ; `local_audio_gapless_chaining_next_track` → `End of gapless continuation` ; `// Flush the resampler` ; `if convolver_format_changed {` ; `let prev_sr = etage.sample_rate` | **la garde la plus exposée** : les bannières sont des ancres ; R7c qui fond les trois boucles fait tomber trois des quatre fenêtres, et le compte `flush_local_dsp(` passe de 6 à 1 ou 2 selon que la queue est rendue dans la boucle commune. La réécrire **avant** la première ligne de R7c, sur le modèle de R1 (positions dans `pousser`, pas comptes de copies) |
| `tune-core/src/audio/resample.rs:1160` | `local.rs` entier | 1 | branche compressée entre `local_audio_compressed_playing` et `Pre-fill the ring buffer` : `rubato_resample_track(` présent, `rubato_resample_batch(` absent | suit la branche compressée, hors périmètre R7c |
| `tune-core/src/outputs/local/backend_fallback_tests.rs:391, 395` | `local.rs` + `resolution.rs` | 1 (sur 21) | pour chacun des quatre marqueurs `_playing` (`local_audio_exclusive_playing` 4665, `local_audio_asio_exclusive_playing` 4933, `wasapi_exclusive_playing` 5487, chemin partagé), une fenêtre qui contient `note_opened_device(` et `"CoreAudio"`/`"ASIO"`/`"WASAPI"`/… | quatre appels `note_opened_device` (4671, 4937, 5492, plus `resolution.rs:338`) deviennent un seul si `SessionOuverte.peripherique_ouvert` est enregistré par le tronc commun ; la garde compte alors un site et non quatre |
| `tune-core/src/outputs/local/chemin_compresse_dsp_tests.rs:16` | `local.rs` | 1 | branche compressée : `apply_local_dsp(` avant `rubato_resample_` | hors périmètre R7c |
| `tune-core/src/outputs/local/cle_de_correlation_i3318.rs:204` | `local.rs` coupé au module | 2 (sur 6) | `journaliser_lecture_lente(` et `journaliser_erreur_de_lecture(` appelées avec `cle_de_flux` et `device_name` (dans `tourner`, 3584, 3609) | suit `tourner` ; si les bras exclusifs entrent dans la boucle commune, ils héritent de la clé (aujourd'hui leurs `read_error` n'en portent pas : 4750, 5202, 5681) |
| `tune-core/src/outputs/local/relache_peripherique_i3575.rs:218` | `local.rs` coupé au module | 3 (sur 13) | `!default_output_config().ok()` ; `decider_la_relache_du_peripherique(` ≥ 2 ; `SentinelleDuFilDeLecture(` ≥ 2 ; `warn!(` avant le détachement | préambule et fil, hors bras |
| `tune-core/src/outputs/local/repli_format_compresse_i3618.rs:140` | `local.rs` coupé au module | 2 (sur 7) | branche compressée : `ouvrir_premier_format_accepte(&tentatives`, `cascade_de_formats(&output_config, &source_config)`, `match ouverture {` … `playing.store(false, Ordering::SeqCst);` contient `open_failure.lock()` et `classify_open_failure(` | hors périmètre R7c ; à réadresser si la cascade devient l'`ouvrir` du backend CPAL |
| `tune-core/src/outputs/local/tests.rs:1442, 1758, 1905` | `local.rs` (+ `parc.rs`) | 3 | 1442 (`l_indice_de_mesure_est_calcule_et_non_ecrit_en_dur`) : dans les 4 000 caractères qui suivent le filtre `.filter(\|c\| c.sample_rate == sample_rate)` (5884), l'appel `sample_rate_evidence_for_device(host_id_name,` (5893) et `endpoint_id = %opened_endpoint_id,` ; 1758 (`la_decision_de_cadence_est_branchee_sur_le_chemin_reel`) : l'appel `decide_local_rate_opening(sample_rate,default_sr,enumerated.is_some(),rate_evidence,)` présent (5894), le court-circuit absent ; 1905 : `pub fn supports_exclusive_mode()` ne se recompose pas en `cfg` | 1442 et 1758 suivent le bloc de décision de cadence (5840–6033), qui devient l'`ouvrir` du backend CPAL ; 1905 est hors `play_url` |
| `tune-core/src/outputs/local/enumeration_asio_occupee_tests.rs:58`, `renseignement_materiel_guard.rs:6`, `tests.rs:1446, 2414`, `backend_fallback_tests.rs:466` | `parc.rs`, `resolution.rs`, `etat_backend.rs` | — | réadressées par R6 ; ne lisent pas `play_url` | rien |

Deux règles qui ressortent, et que #3981 a déjà appliquées :

- une garde qui **coupe** au premier `#[cfg(test)]\nmod tests` suit le
  fichier où ce module vit ; elle s'érode si un module de test est ajouté
  plus haut ;
- une garde qui affirme une **absence** (`!LOCAL.contains(".emit(")`,
  `!avant_tampon.contains("rubato_resample_batch(")`) ne peut pas être
  réadressée par remplacement du fichier lu : elle perd du périmètre en
  silence. Lire l'ancien fichier **et** le nouveau.

Dix-sept lecteurs de `local.rs` sur cette tête, dont neuf hors du dossier
`local/` (sept dans `tune-server/tests`, `tune-core/tests/dsp_track_boundary.rs`,
`tune-core/src/audio/resample.rs`) ; c'est le compte que
`scripts/refonte/gardes.sh` doit rendre identique avant et après chaque
tranche.

## 6. Refaire le relevé

```bash
git fetch origin fix/2219-r6-decoupe-local-b209
git worktree add /tmp/wt FETCH_HEAD --detach
awk 'NR>=3700 && /^    (pub(\([a-z]+\))? )?(async )?fn /' /tmp/wt/tune-core/src/outputs/local.rs   # bornes des méthodes
sed -n '3788,7195p' /tmp/wt/tune-core/src/outputs/local.rs | grep -n '#\[cfg\|^            // ------- '     # bras et cfg
git -C /tmp/wt grep -n 'include_str!\|read_to_string' -- tune-core tune-server | grep -i local           # gardes
```
