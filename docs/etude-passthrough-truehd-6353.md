# Étude #6353 — passthrough TrueHD / Dolby Digital Plus des MKV vers un ampli

*Étude seule, aucun code de production. Base lue : `batch/bugs-rc8-20261022`
(`aba072dc2`). Date : 23/10/2026. Demande de Didier (« Gros Bidon »), forum,
fil 2202, réponse 8312 du 14/10 ; ancienne demande : #3633 (fermée).*

## 0. Résumé

- **Le passthrough ne demande aucun décodeur Dolby.** Tune lit la piste,
  l'emballe en IEC 61937 et l'envoie comme un faux PCM 16 bits, avec le bit
  « non audio » levé. C'est l'ampli qui décode. Cela contourne le mur de
  #3633 (pas de décodeur Rust, pas de ffmpeg).
- **Ce qui existe déjà.** L'« étape 1 » a été fusionnée le 20/10 (PR #6632)
  et elle est dans `v1.0.0-rc7`. Elle se trouve dans `tune-core/src/audio/flux_dolby.rs`
  (594 lignes) : elle reconnaît les flux AC-3, E-AC-3 et TrueHD d'une piste
  Matroska et emballe une trame AC-3 ou E-AC-3. Elle ne branche **aucune
  sortie**. Le seul effet visible est le motif du rapport de scan. Elle a été
  écrite **avant** la décision du 23/10 (« étude d'abord ») : ce document en
  tient compte, sans la défaire.
- **Les sorties qui peuvent porter le flux** :

  | Sortie | AC-3 | E-AC-3 (DD+) | TrueHD (Atmos) |
  |---|---|---|---|
  | HDMI | oui | oui | oui, en HBR |
  | S/PDIF | oui | non en pratique | non |
  | Linux ALSA (`hdmi:`, `iec958:`, `hw:`) | oui | oui | oui |
  | Windows WASAPI exclusif | oui | oui | oui |
  | macOS CoreAudio | probablement, en S/PDIF ou HDMI | non établi | **non** |
  | ASIO | à la marge, selon le pilote | non | non |
  | DLNA, AirPlay, Chromecast | non | non | non |

  Linux et Windows, via WASAPI exclusif, sont les deux seuls systèmes où les
  trois formats sont atteignables.
- **TrueHD est nettement plus dur que DD+.** Il faut le regroupement **MAT**,
  qui n'a pas de documentation publique. Il n'existe que des réalisations
  rétro-ingéniérées, sous LGPL (ffmpeg) ou GPL (Kodi, dérivé de LAV Filters).
  Tune est sous BSL : on ne peut pas recopier ce code.
- **Volume : c'est la zone sensible.** Un flux compressé ne s'atténue pas et
  ne s'égalise pas. Le volume devient celui de l'ampli, et une limite de
  sécurité de #6541 ne peut plus rien borner. Un bit « non audio » ignoré par
  la chaîne donnerait du **bruit à pleine échelle**.
- **Recommandation : faire une partie** (§8). On commence par AC-3 et E-AC-3,
  sur la sortie locale Linux et Windows. La fonction serait à activer par
  zone, et seulement sur une zone en volume fixe. TrueHD ne viendrait qu'après
  un essai réel chez Didier ou un autre testeur équipé, et sur une
  réécriture du MAT d'après le comportement observé. macOS et le réseau sont
  exclus.

## 1. Ce que Tune a déjà

### 1.1 Démultiplexage

- `symphonia-format-mkv` 0.6.0 (MPL-2.0, déjà dans le `Cargo.lock`, fonction
  `mkv` du `Cargo.toml` l. 92) connaît les pistes Dolby :
  `codecs.rs` l. 265-267 (`A_AC3`, `A_EAC3`, `A_TRUEHD`). Il rend leurs paquets
  bruts par `next_packet`, et sait chercher une position par les `Cues`
  (`demuxer.rs` l. 393-410).
- `tune-core/src/audio/matroska.rs` : `sonder` (l. 252) puis
  `lire_le_flux_dolby` (l. 318). Cette fonction lit au plus 256 paquets
  (`PAQUETS_POUR_LE_FLUX`, l. 314), parce que TrueHD ne répète sa
  synchronisation majeure que toutes les 128 unités d'accès au plus.

### 1.2 `flux_dolby.rs` (#6632)

| Fonction | Ligne | Ce qu'elle fait |
|---|---|---|
| `analyser_ac3` | 218 | en-tête AC-3 / E-AC-3 (A/52, annexe E), sous-flux dépendants |
| `analyser_truehd` | 385 | synchronisation majeure `0xF8726FBA`, fréquence, 6 ou 8 canaux, nombre de sous-flux |
| `analyser_les_paquets` | 436 | verdict sur une suite de paquets |
| `porteuse_iec61937` | 481 | AC-3 : 2 canaux à fs ; E-AC-3 : 2 canaux à 4 × fs ; TrueHD : 8 canaux à 4 × fs |
| `rafale_iec61937` | 515 | préambules `Pa`/`Pb`/`Pc`/`Pd`, trame en mots, bourrage ; `None` pour TrueHD |
| `chemin_du_signal` | 558 | les étapes du signal ; le transport y est déclaré « non pris en charge » |

Seul `support.rs` l. 167 (`refus_matroska`) s'en sert : il produit le motif
« son envoi tel quel à un amplificateur (passthrough) n'est pas encore pris en
charge (#6353) ». Aucune autre fonction n'est appelée hors des tests.

**Trois limites relevées à la lecture.** Ce ne sont pas des défauts visibles,
puisque rien n'émet encore :

1. **E-AC-3 : une trame par rafale.** IEC 61937 veut une rafale par période de
   1 536 échantillons (6 blocs). Une trame E-AC-3 de 1, 2 ou 3 blocs
   (`numblkscod` < 3) doit être **cumulée** avec les suivantes, de même que
   ses sous-flux dépendants. C'est ce que fait ffmpeg (`spdifenc.c`,
   `eac3_repeat[4] = {6, 3, 2, 1}`). `analyser_ac3` saute `numblkscod`
   (l. 267). Le cas courant (DD+ à 48 kHz, 6 blocs par trame) passe ; les
   autres non.
2. Le commentaire de `SYNC_TRUEHD` (l. 355) écrit `0xF8726FBB`, la valeur de
   MLP, alors que la constante vaut `0xF8726FBA`, celle de TrueHD. C'est la
   constante qui est juste.
3. **Compression d'en-tête Matroska.** D'anciens `mkvmerge` retiraient les
   octets de synchronisation des trames AC-3 et DTS (`ContentCompression`,
   « header removal »). `symphonia-format-mkv` 0.6.0 déclare ces éléments
   (`schema.rs` l. 184-191) mais **ne les applique pas** : rien hors du
   schéma ne les lit. Un tel fichier rendrait des trames sans `0x0B77`, et
   la sonde ne le reconnaîtrait pas. Il faudrait recoller l'en-tête retiré,
   ce que symphonia ne fait pas.

### 1.3 Le précédent DoP : le modèle à suivre

Le DoP est déjà un « faux PCM » qui doit arriver intact. Tune sait le
protéger :

- `outputs/local.rs` l. 845 : `effective_volume_units(…, dop)` rend `1000`
  sous DoP. Le curseur ne fait rien, et c'est voulu (« Silently inert is a
  better failure than silently mute »).
- l. 3673 et l. 3712 : DSP et vidage du convolueur sont sautés
  (`if dop || pure_bypass`).
- l. 3899 et l. 3988 : le chemin du signal affiche `BypassedDop` pour le DSP
  et le volume.
- `bras_alsa_natif.rs` (l. 1-45) : l'anneau `f32` rend les mots 16 et 24 bits
  **intacts** quand le volume vaut exactement 1. Une rafale IEC 61937 en
  16 bits y survit donc, à condition que **rien** ne la touche.

Une rafale IEC 61937 demande la même protection que le DoP, et **plus**
(§5.2). Mais elle ne doit pas être *détectée* à la façon du DoP
(`is_dop_pcm`, l. 2798). Elle doit être *déclarée* par le décodeur : une
fausse détection sur de la musique serait sans danger dans un sens, mais pas
dans l'autre.

## 2. Les sorties capables

### 2.1 Les liaisons

- **HDMI.** Il porte AC-3 et E-AC-3 sur une porteuse 2 canaux, et TrueHD en
  **HBR** (High Bit Rate) : 8 canaux à 192 kHz en 16 bits, soit
  24,576 Mbit/s, pour un TrueHD qui monte à environ 18 Mbit/s. Le noyau Linux
  le dit en une ligne : « HBR should be Non-PCM, 8 channels »
  (`sound/hda/codecs/hdmi/hdmi.c`, `is_hbr_format`). La broche doit avoir
  `AC_PINCAP_HBR`, faute de quoi `hdmi_setup_stream` répond « HBR is not
  supported ».
- **S/PDIF (IEC 60958, coaxial ou optique).** Il porte de l'IEC 61937 sur
  2 canaux. L'AC-3 tient à 48 kHz : 1,536 Mbit/s de porteuse, pour 640 kbit/s
  au plus de flux. L'E-AC-3 exige une porteuse à 4 × fs, soit 192 kHz
  stéréo. La norme ne l'interdit pas, mais les entrées S/PDIF des amplis ne
  l'acceptent pas en pratique, et Kodi ne propose le DD+ qu'en HDMI. Je n'ai
  pas de texte normatif pour cette dernière affirmation (§7). TrueHD est
  **impossible** en S/PDIF : il lui faut 8 canaux à 192 kHz.
- **Atmos** voyage *dans* TrueHD (4 sous-flux) ou dans E-AC-3 (métadonnées
  JOC/EMDF). Le passthrough le transmet sans le savoir. L'ampli doit savoir
  le décoder.

### 2.2 Linux, ALSA

- **Les noms de PCM.** `hdmi:CARD=x,DEV=y,AES0=…,AES1=…,AES2=…,AES3=…` et
  `iec958:…` acceptent les quatre octets d'état de canal
  (`alsa-lib/src/conf/pcm/hdmi.conf`, `@args [ CARD DEV AES0 AES1 AES2 AES3 ]` ;
  `AES0` vaut `0x04` par défaut, « consumer, not-copyright »). Les fichiers
  de carte les écrivent dans le contrôle **`IEC958 Playback Default`** par un
  crochet `ctl_elems` (`cards/HDA-Intel.conf` l. 113-121).
- **Le bit « non audio »** est `AES0=0x06`. Le pilote HDA le relit
  (`check_non_pcm_per_cvt`) pour décider du mode non-PCM et de l'infoframe.
  Kodi utilise `AES0=0x06,AES1=0x82,AES2=0x00`, puis `AES3` selon la
  porteuse : `0x02` à 48 kHz, `0x0e` à 192 kHz, `0x09` pour une porteuse HBR
  en 8 canaux (`AESinkALSA.cpp`, `GetAESParams`).
- **Avec `hw:`** (la sortie de Tune la plus directe), il n'y a pas
  d'arguments : il faut écrire soi-même le contrôle `IEC958 Playback Default`
  de la carte (`alsa::ctl`), puis ouvrir le PCM en `S16_LE`.
- **Dans Tune.** cpal ouvre un PCM par son nom (`vendor/cpal/src/host/alsa/mod.rs`
  l. 355 et 457, `alsa::pcm::PCM::new(&self.pcm_id, …)`), mais il n'énumère
  que les noms que l'indice ALSA lui donne, sans arguments AES. Pour le
  passthrough, il faudrait l'une des deux voies suivantes :
  - un bras de sortie qui ouvre lui-même le PCM `hdmi:…,AES0=0x06,…` par la
    caisse `alsa` 0.11, déjà tirée par cpal ;
  - l'écriture du contrôle IEC958 avant l'ouverture `hw:` par cpal, puis sa
    restauration à la fermeture.

  Le filtre de périphériques garde déjà `iec958:` et `hdmi:`
  (`pseudo_peripherique_alsa.rs` l. 50-51).
- **PipeWire ou PulseAudio** tiennent souvent la carte. Le passthrough exige
  l'accès direct, comme le bit-perfect aujourd'hui.
- **Tune OS (Raspberry Pi)** n'a plus de sortie audio locale depuis le 20/05
  (`pseudo_peripherique_alsa.rs` l. 52-53). Les témoignages sur le
  passthrough HBR du pilote `vc4-hdmi` se contredisent (LibreELEC 10 :
  « fonctionne », d'autres : « aucun son »). **Hors périmètre.**

### 2.3 Windows, WASAPI exclusif

- Le format est un `WAVEFORMATEXTENSIBLE_IEC61937` : un
  `WAVEFORMATEXTENSIBLE`, plus `dwEncodedSamplesPerSec`,
  `dwEncodedChannelCount` et `dwAverageBytesPerSec` (Microsoft Learn,
  `ksmedia.h`). Le `SubFormat` vaut :
  - `KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_DIGITAL` pour l'AC-3 ;
  - `…_DOLBY_DIGITAL_PLUS` pour l'E-AC-3, en 2 canaux à 192 kHz ;
  - `…_DOLBY_MLP` pour TrueHD, en 8 canaux à 192 kHz.

  C'est ce que fait Kodi (`AESinkWASAPI.cpp` l. 442 à 499, et
  `BuildWaveFormatExtensibleIEC61397` l. 637).
- **Dans Tune.** `wasapi_exclusive.rs` a sa propre FFI : `WAVEFORMATEXTENSIBLE`
  l. 153, un seul `KSDATAFORMAT_SUBTYPE_PCM` l. 90, le format d'un candidat
  l. 334-365 (`cbSize: 22`, `SubFormat: …_PCM`) et une sonde
  `IsFormatSupported(EXCLUSIVE)` l. 367-379. Il faudrait ajouter la structure
  étendue (`cbSize` plus grand), trois GUID et un candidat « IEC 61937 »
  hors de la négociation PCM (#3837). Le travail est local et le chemin est
  connu.
- WASAPI partagé ne convient pas : le mélangeur de Windows n'accepte pas ces
  formats.

### 2.4 macOS, CoreAudio

- **Ce qui est possible.** VLC et mpv font de l'AC-3 (et du DTS) en S/PDIF
  ou en HDMI par le « hog mode » et un format physique
  `kAudioFormat60958AC3`, quand le flux du périphérique le propose (mpv,
  `ao_coreaudio_utils.c`, qui le traite comme « digital » ; commits VLC
  `auhal` de 2011 et 2017).
- **Ce qui ne l'est pas.** TrueHD n'est pas possible : aucun format physique
  public ne correspond au HBR. Un mainteneur de Kodi l'écrit en 2017 :
  « there was never the possibility to pass through any HD formats on Mac
  OS » (forum Kodi, fil 314399). La même réponse revient en 2014. Pour
  l'E-AC-3 en passthrough, je n'ai trouvé aucune source fiable : c'est **non
  établi**. Les échos d'une API de passthrough récente chez Apple
  (forum Firecore, juin 2025) concernent tvOS et restent des conjectures.
- **Dans Tune.** `coreaudio_exclusive.rs` prend déjà le hog mode (l. 6, 103-104),
  mais son format physique est du PCM linéaire seulement (l. 37,
  `RequestedPhysicalFormat` l. 93). **Verdict : ne pas faire macOS**, ou
  seulement l'AC-3 plus tard, s'il y a une demande.

### 2.5 ASIO

L'API ASIO ne connaît que des types d'échantillons PCM et DSD. Elle n'a pas
de format compressé ni de commande pour l'état de canal. On peut écrire une
rafale comme du PCM 16 bits sur une sortie S/PDIF, mais le bit « non audio »
dépend du panneau du pilote : RME, par exemple, a une case « Non-Audio ».
Tune ne peut ni le poser ni le vérifier. Faute de ce bit, l'ampli reçoit du
bruit à pleine échelle. HDMI par ASIO n'existe pas en pratique. **Verdict :
non.**

### 2.6 Sorties réseau

- **AirPlay.** Le protocole audio que Tune réalise transporte de l'ALAC, de
  l'AAC ou du PCM vers un récepteur qui décode. Il n'y a pas de charge utile
  « flux compressé à transmettre ». **Non.**
- **Chromecast.** Le récepteur Web annonce le passthrough AC-3 et E-AC-3,
  mais en MP4, MP2T ou WebM, **pas en Matroska**, et **pas de TrueHD**. Le
  Google TV Streamer note même Atmos « (no passthrough) » (Google,
  « Supported Media »). Il faudrait remultiplexer, et seul un Chromecast
  branché en HDMI sur l'ampli en profiterait. **Non**, pour un gain marginal.
- **DLNA.** Ce ne serait pas de l'IEC 61937 : ce serait servir le fichier
  (ou la piste extraite, `audio/eac3`) à un renderer qui l'annonce dans son
  `GetProtocolInfo` (`dlna.rs` l. 531). Les amplis audio (Denon AVR-X1600H,
  mesure de #4573) n'annoncent que du LPCM et du FLAC 2 canaux. Seuls des
  téléviseurs ou des boîtiers vidéo le feraient. **Non** dans ce chantier.
  Ce serait une autre fonction (« URL directe d'un MKV vers un renderer
  vidéo »).

## 3. Savoir ce que l'ampli accepte

- **Linux, l'ELD.** Le pilote HDMI expose l'ELD de l'écran ou de l'ampli :
  contrôle `ELD` (interface PCM) et `/proc/asound/cardN/eld#…`. Ses
  descripteurs audio courts (CEA-861) donnent les codages :
  `2 "AC-3"`, `10 "E-AC-3/DD+ (Dolby Digital Plus)"`, `12 "MLP (Dolby TrueHD)"`
  (`sound/core/pcm_drm_eld.c`, `cea_audio_coding_type_names`). Kodi le lit
  (`AESinkALSA.cpp` l. 1418-1440). Mais il prévient que la détection « can go
  wrong on Intel, Nvidia and on all AMD » et garde le périphérique ouvert
  même sans ELD.
- **Windows, `IsFormatSupported`.** C'est la seule sonde. Kodi lui-même ne
  s'y fie pas sur HDMI : il **ajoute** TrueHD et E-AC-3 même quand la sonde
  échoue (`if (SUCCEEDED(hr) || details.eDeviceType == AE_DEVTYPE_HDMI)`,
  `AESinkWASAPI.cpp` l. 444 et 461, avec le journal « seems to be not
  supported »).
- **S/PDIF.** Il n'a **aucun canal de retour**. Rien ne dit si l'ampli
  décode.
- **Ce que Tune sonde déjà.** Les canaux annoncés par un renderer DLNA
  (`canaux_reseau_4573.rs`, « une déclaration, jamais une supposition »), et
  les cadences et canaux d'une carte locale (`s32_annonce`,
  `bras_alsa_natif.rs` l. 200). Rien sur les codages compressés.
- **Ce qui reste à l'utilisateur.** C'est à lui de **déclarer** que la zone
  sort vers un ampli qui décode, et quels formats elle accepte (AC-3, DD+,
  TrueHD). L'ELD peut pré-cocher et avertir (« l'ampli n'annonce pas
  TrueHD »), jamais trancher seul. C'est le même principe que #4573 : on
  déclare, on ne force pas.

## 4. Lire les MKV

### 4.1 Démultiplexage, en Rust pur

| Caisse | Licence | État | Usage |
|---|---|---|---|
| `symphonia-format-mkv` 0.6.0 | MPL-2.0 | déjà là | **suffit** : pistes Dolby, paquets, `Cues` |
| `matroska-demuxer` 0.8.1 | Zlib/MIT/Apache-2.0 | maj 08/2026 | repli possible ; non essayé |
| `matroska` 0.30.1 | — | métadonnées seules | non |
| `truehd` 0.7.2 | Apache-2.0 | « Research implementation », créée 07/2025 | inutile : on ne décode pas |

Il ne faut ni ffmpeg ni nouvelle dépendance pour démultiplexer. Il reste
deux points à traiter :

- **Le choix de la piste.** Un film a souvent plusieurs pistes : TrueHD
  Atmos, AC-3 de compatibilité, langues. La sonde actuelle prend la piste
  par défaut (`default_track`, `matroska.rs` l. 330). Il faudrait choisir
  selon la zone : la meilleure piste que l'ampli accepte, avec un repli sur
  AC-3 vers S/PDIF.
- **La compression d'en-tête** : §1.2, point 3.

### 4.2 Les trames

- **AC-3 et E-AC-3.** Le bloc Matroska porte une ou plusieurs trames
  entières, que `0x0B77` et `frmsiz` délimitent. Le travail est simple, et
  `flux_dolby.rs` le fait déjà.
- **TrueHD.** Le bloc porte des unités d'accès (40 échantillons à 48 kHz,
  soit 1/1200 s), avec une synchronisation majeure au moins toutes les 128.
  Leur longueur est dans l'en-tête (`flux_dolby.rs` l. 413).

### 4.3 L'emballage IEC 61937

| | AC-3 | E-AC-3 | TrueHD |
|---|---|---|---|
| `Pc` (type) | 1 | 21 | 22 (MAT) |
| Porteuse | 2 canaux, fs | 2 canaux, 4 × fs | 8 canaux, 4 × fs (HBR) |
| Rafale | 6 144 o (1 536 trames) | 24 576 o | 61 440 o |
| Durée de rafale | 32 ms | 32 ms | 20 ms (24 unités) |
| `Pd` | longueur en bits | en octets | en octets |

Une rafale commence par `Pa=0xF872` et `Pb=0x4E1F`. Elle porte ensuite les
données en mots 16 bits, puis du **bourrage** à zéro jusqu'à la période. Le
mot de 16 bits doit partir dans l'ordre que la liaison attend (le petit-boutiste
de `rafale_iec61937`). Entre deux pistes, ou en pause, il faut des rafales de
**pause** (type 3) ou du silence numérique, sans quoi l'ampli se recale ; ce
point est à mesurer.

**MAT pour TrueHD.** On regroupe 24 unités d'accès dans une trame de
61 424 octets. Elle porte trois codes fixes : un début de 20 octets à la
position 0, un milieu de 12 octets à 30 708 et une fin. Les unités y sont
**placées selon leur `input_timing`**, avec un bourrage calculé entre elles,
et non serrées. ffmpeg le dit franchement : « It seems Dolby TrueHD frames
have to be encapsulated in MAT frames », et Kodi : « magic MAT format values,
meaning is unknown at this point » (`PackerMAT.cpp`). Kodi a dû réécrire son
emballeur (2024, d'après LAV Filters) pour les « seamless branches » des
Blu-ray, détectées par `outputTiming`. C'est la partie la plus risquée du
chantier :

- aucune spécification publique ;
- un ampli peut se taire ou claquer sur une trame MAT mal placée ;
- on ne peut écrire une réalisation que d'après le comportement décrit, sans
  copier du LGPL ou du GPL dans un dépôt BSL.

### 4.4 Synchronisation, avance et recul

- **La position.** Tune ne la voit plus en échantillons PCM décodés. Il faut
  la compter en rafales : 1 536 échantillons pour l'AC-3 et l'E-AC-3, 960
  pour un MAT à 48 kHz. Pour TrueHD, il faut aussi suivre l'horloge du flux,
  puisque le MAT bourre selon le timing.
- **Le déplacement.** On cherche par les `Cues` (`seek_track_by_ts`), puis
  on repart sur une trame AC-3 ou E-AC-3 entière (et indépendante pour
  l'E-AC-3), ou sur une **synchronisation majeure** TrueHD. Il faut au plus
  128 unités, soit environ 107 ms. L'emballeur MAT est remis à zéro.
- **Le gapless** entre deux MKV n'a pas de sens, et l'ampli se recale à
  chaque changement de format. Pas de fondu enchaîné possible (§5.2).
- **Pas de synchronisation image** : Tune ne lit que l'audio.

## 5. Ce que ça change dans Tune

### 5.1 Où cela s'insère

Le chemin serait le suivant :

`MKV → démuxeur (symphonia) → trames Dolby → emballeur IEC 61937 → « porteur »
16 bits, 2 ou 8 voies → bras de sortie exclusif (ALSA hdmi/iec958 avec
AES0=0x06, ou WASAPI IEC61937) → ampli`

Un **nouveau type de source** remplacerait le décodeur PCM sur ce chemin. Il
rendrait des octets « WAV » 16 bits, comme le DoP rend du 24 bits, avec une
**marque explicite** « porteur IEC 61937 » qui suit le tampon jusqu'au bras.
Le bras serait dédié, sur le modèle de `bras_alsa_natif.rs` : il prend la
main seulement si toutes les conditions sont réunies, et sinon il **refuse**
la lecture avec un code stable. Il ne doit **jamais** se replier sur le
chemin partagé, qui jouerait la rafale comme de la musique.

### 5.2 Ce qui doit être court-circuité

Le DoP court-circuite déjà le volume et le DSP (§1.3). Le passthrough exige
**en plus** de couper tout ce qui touche un mot de 16 bits :

- volume utilisateur, ReplayGain, part de compensation de l'égaliseur
  (`effective_volume_units`, l. 845) ;
- égaliseur, convolueur, crossfeed, AutoEQ, réduction mono
  (`apply_local_dsp`, l. 3702) ;
- **limiteur** de sécurité (`audio/limiteur.rs`, #5171) ;
- rampe de silence (`soft_mute.rs`) en pause, arrêt ou déplacement : il faut
  couper net sur une **frontière de rafale** ;
- fondu enchaîné (`fondu_enchaine.rs`), fondu de zone ;
- rééchantillonnage, tramage, réaffectation et carte des canaux (#6171,
  `reaffectation_canaux.rs`), réduction de canaux (#4573) ;
- mixage des alarmes ou d'annonces par-dessus le flux.

Une seule de ces étapes active, et la rafale est cassée. Si le bit « non
audio » passe, l'ampli se tait ou claque. S'il ne passe pas, il joue du
bruit blanc **à pleine échelle**.

### 5.3 Volume : zone sensible

- **Le volume devient celui de l'ampli.** Tune ne peut ni l'atténuer ni le
  borner.
- **#6541.** La limite de sécurité « que Tune ne dépasse jamais » ne peut
  plus rien faire sur un flux compressé. La règle déjà décidée le 21/10 pour
  PURE et le volume fixe (`volume_limites.rs` l. 26-35 : une limite « ne se
  POSE pas » sur ces zones) donne la bonne forme :
  - le passthrough ne s'active que sur une zone **en volume fixe** ;
  - il est refusé, avec un code stable, sur une zone qui porte une limite de
    sécurité ;
  - l'interface affiche « volume : sur l'ampli ».

  Le contraire serait une limite affichée qui ne protège rien.
- **PURE.** Il est compatible, puisque PURE ne touche déjà rien. Le
  passthrough est en un sens « plus PURE que PURE », mais il ne doit **pas**
  dépendre de PURE : PURE non verrouillé laisse le volume actif
  (`audiophile.rs` l. 10).
- **Le bruit pleine échelle.** Le premier essai de chaque installation doit
  se faire **ampli au minimum**. Le document d'aide doit le dire.
- **La commande du volume de l'ampli** (CEC, API réseau Denon ou Marantz…)
  serait la seule façon de rendre un curseur à l'utilisateur. Hors
  périmètre.

### 5.4 Interface

- **Le réglage de zone** : « sortie vers un ampli décodeur (passthrough) »,
  avec les formats acceptés, pré-cochés par l'ELD quand il existe.
- **Le chemin du signal** : il réutilise `chemin_du_signal` (`flux_dolby.rs`
  l. 558). Il affiche « Dolby TrueHD 7.1, Atmos (4 sous-flux) → HDMI HBR →
  décodé par l'ampli », avec le volume et le DSP « court-circuités ».
- **Les curseurs** de volume et d'égaliseur sont grisés, avec la raison.
- **Le nom des formats.** On décrit le codec (« Dolby TrueHD ») sans revendiquer
  une certification Dolby (§7).

### 5.5 Bibliothèque

- Aujourd'hui, un MKV dont la piste est en Dolby est **refusé** et compté,
  avec son motif (`support.rs` l. 167). Avec le passthrough, il entrerait en
  bibliothèque, marqué « passthrough seulement ». Il ne serait lisible que
  sur une zone déclarée capable.
- **Choisir la piste** par langue, codec et canaux : il faut lire **toutes**
  les pistes audio, pas seulement la piste par défaut.
- **La piste vidéo** est ignorée. Le fichier d'un film est grand : pas
  d'analyse PCM (empreinte, crêtes, ReplayGain, ondes), puisqu'il n'y a pas
  de décodeur.
- **Les métadonnées** : balises Matroska, chapitres. Les chapitres pourraient
  servir de « pistes » d'un concert, mais c'est une autre demande.

### 5.6 Les tests

**Sans matériel :**

- des témoins sur des trames fabriquées d'après les normes. `flux_dolby_tests.rs`
  et le mini-muxer de `matroska.rs` le font déjà, sans fichier versionné et
  sans ffmpeg ;
- **un faux périphérique** : le greffon ALSA `file` ou `null` avec capture,
  ou `snd-aloop`. On vérifie, octet par octet, que les rafales arrivent
  intactes, à la bonne période, avec `Pa`/`Pb` alignés, et que le contrôle
  IEC958 porte `AES0=0x06` ;
- un relecteur indépendant : `ffmpeg -f spdif` en **outil de test hors
  dépôt** seulement, jamais en dépendance de lecture. Sinon, un analyseur de
  rafales écrit pour le test ;
- des épreuves négatives : volume ≠ 1, égaliseur armé, limite de sécurité
  posée, zone non déclarée. Chaque cas doit **refuser**, pas jouer.

**Avec matériel :**

- un ampli HDMI qui affiche le format reçu : Denon ou Marantz, comme le
  X1600H de Xavier, ou Yamaha, qui annoncent « Dolby TrueHD » ou « Dolby
  Atmos » en façade ;
- un PC Linux avec une carte graphique Intel ou Nvidia en HDMI (HBR), et un
  PC Windows en WASAPI exclusif ;
- **le matériel de Didier**, inconnu à ce jour (§7). C'est lui qui a la
  demande et les fichiers ;
- pour le S/PDIF, n'importe quel ampli avec une entrée optique, pour
  l'AC-3.

## 6. Étapes, taille et risques

Ces tailles sont des estimations : rien n'a été prototypé.

| Étape | Contenu | Taille (tests compris) | Prérequis |
|---|---|---|---|
| A | E-AC-3 cumulé à 6 blocs, choix de piste, en-tête retiré (§1.2) | 300-500 l. | — |
| B | Source « porteur IEC 61937 » et marque jusqu'au bras ; court-circuits du §5.2 ; refus stables | 700-1 000 l. | A |
| C | Bras ALSA : `hdmi:`/`iec958:` avec AES, ou contrôle IEC958 + `hw:` ; ELD lu | 600-900 l. | B |
| D | Bras WASAPI : `WAVEFORMATEXTENSIBLE_IEC61937`, GUID, sonde | 400-600 l. | B |
| E | Zone « ampli décodeur », volume fixe obligatoire, #6541, interface, chemin du signal | 400-700 l. (serveur + web) | B |
| F | Bibliothèque : MKV Dolby admis « passthrough seulement », pistes multiples | 300-500 l. | A |
| G | **TrueHD** : emballeur MAT d'après le timing, HBR 8 canaux, déplacement sur synchro majeure | 700-1 200 l. | C ou D, **et un essai réel** |

Les étapes A à F représentent environ 2 700 à 4 200 lignes, et G environ
700 à 1 200 de plus.

### Risques

- **Le bruit à pleine échelle** si le bit « non audio » ne passe pas : PCM
  ouvert par un mauvais nom, pilote qui ignore le contrôle, ASIO. C'est le
  risque n° 1 : il touche l'ouïe et le matériel.
- **Le MAT** : non documenté, sensible au timing, et seulement vérifiable
  avec un vrai décodeur.
- **Les pilotes HDMI** : ELD absent ou faux (Kodi le dit), HBR refusé par la
  broche, `IsFormatSupported` qui ment sur HDMI.
- **La concurrence d'accès** : PipeWire tient la carte, WASAPI exclusif est
  pris par une autre application.
- **La surface** : une famille de court-circuits de plus, à tenir en
  cohérence avec chaque nouvel effet DSP. Une garde de texte est à prévoir.
- **Les licences** : on ne copie aucun code ffmpeg (LGPL) ni Kodi (GPL).

## 7. Ce qui n'est pas établi

- **Les fichiers et le matériel de Didier.** Ses MKV « Atmos » sont-ils en
  TrueHD (rips Blu-ray) ou en DD+ (sources web) ? Quel ampli, et sur quelle
  entrée (HDMI ou optique) ? Quelle machine fait tourner Tune ? Tout le
  partage entre les étapes A-F et l'étape G en dépend. C'est la question à
  lui poser avant d'engager quoi que ce soit.
- **L'E-AC-3 en S/PDIF** : refusé « en pratique », sans texte normatif lu ici.
  La norme IEC 61937 est payante et n'a pas été consultée : les valeurs du
  §4.3 viennent de ffmpeg et de Kodi, et elles concordent.
- **macOS et l'E-AC-3** : aucune source fiable, ni dans un sens ni dans
  l'autre.
- **Le comportement des amplis** quand une rafale manque, entre deux pistes
  ou après un déplacement : faut-il des rafales de pause, et combien ?
- **Les obligations juridiques**. Je n'ai trouvé aucune exigence de licence
  Dolby pour *transmettre* un flux sans le décoder : Kodi et VLC le font. Je
  ne les ai pas fait vérifier pour autant. L'usage des marques (« Dolby »,
  « Atmos ») dans l'interface et la publicité est à cadrer.
- **Le Raspberry Pi (`vc4-hdmi`) en HBR** : les témoignages se contredisent.
- **La part des MKV à en-tête retiré** dans les bibliothèques réelles.
- **La taille des étapes** : estimée, sans prototype.

## 8. Recommandation

**Faire une partie, en deux temps, et seulement après la réponse de Didier.**

1. **Oui, mais pas avant la rc8 : AC-3 et E-AC-3 en local (étapes A à F).**
   Sur Linux (ALSA HDMI et S/PDIF pour l'AC-3) et sur Windows (WASAPI
   exclusif). Le chemin est connu, documenté par Microsoft et ALSA, et
   vérifiable sur un faux périphérique. La fonction s'active par zone, sur
   une zone en volume fixe et sans limite de sécurité. Le refus est franc
   partout ailleurs.
2. **TrueHD (étape G) : seulement si Didier confirme du TrueHD et peut
   tester.** C'est un MAT réécrit d'après le comportement observé, essayé
   chez lui avant toute publication. Sans testeur équipé, on ne livre pas :
   un emballeur MAT faux ne se voit qu'à l'oreille, sur un vrai décodeur.
3. **Non : macOS, ASIO, et toutes les sorties réseau** (AirPlay, Chromecast,
   DLNA). La plateforme ou le protocole ne le permet pas, ou le gain est
   marginal (§2.4 à 2.6).

Si Didier n'a que du TrueHD Atmos et ne peut pas tester, la seule réponse
honnête est **ne pas faire maintenant**. Le premier temps ne lui servirait
pas.

## Sources

- Code de Tune (`aba072dc2`) : `tune-core/src/audio/flux_dolby.rs`,
  `matroska.rs`, `support.rs`, `volume_limites.rs`, `audiophile.rs`,
  `outputs/local.rs`, `outputs/local/bras_alsa_natif.rs`,
  `outputs/wasapi_exclusive.rs`, `outputs/coreaudio_exclusive.rs`,
  `outputs/pseudo_peripherique_alsa.rs`, `audio/canaux_reseau_4573.rs`,
  `vendor/cpal/src/host/alsa/mod.rs`.
- `symphonia-format-mkv` 0.6.0 : `src/codecs.rs`, `src/demuxer.rs`,
  `src/schema.rs`.
- Linux : `sound/hda/codecs/hdmi/hdmi.c` (`is_hbr_format`,
  `hdmi_pin_hbr_setup`) ; `sound/core/pcm_drm_eld.c`
  (`cea_audio_coding_type_names`).
- alsa-lib : `src/conf/pcm/hdmi.conf` ; `src/conf/cards/HDA-Intel.conf`.
- Microsoft Learn : `WAVEFORMATEXTENSIBLE_IEC61937` (ksmedia.h) ;
  `WAVEFORMATEXTENSIBLE`.
- Kodi (xbmc/xbmc, master) : `AESinkALSA.cpp`, `AESinkWASAPI.cpp`,
  `Utils/PackerMAT.cpp`. Lus pour comprendre, non recopiés (GPL).
- FFmpeg : `libavformat/spdifenc.c`. Lu pour comprendre, non recopié (LGPL).
- Google Cast, « Supported Media » (passthrough AC-3/E-AC-3, conteneurs).
- Forum Kodi, fils 185454 et 314399 (macOS sans passthrough HD) ; mpv,
  `ao_coreaudio_utils.c` ; VLC, commits `auhal` (2011, 2017).
- crates.io : `matroska-demuxer`, `matroska`, `truehd` (consulté le
  23/10/2026).
